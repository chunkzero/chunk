//! One player's connection: routed by its handshake, then either answered with a status or spliced to a gateway,
//! waking a sleeping environment first.

use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use chunk_management::v1::{SleepingPingMode, WakeReason};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::{Instant, sleep, timeout},
};

use crate::{
    Shared, gateway,
    handshake::{self, Handshake, Hello, Intent},
    limits::Permit,
    proxy_header,
    routes::Entry,
    status::{self, Fetch},
    wake,
    wire::{self, Frames, invalid},
};

/// What a login may have sent in all by the time it is spliced, handshake included.
const MAX_HELD: usize = 8192;
const RELAY_BUFFER: usize = 8192;
/// How long a relayed write may make no progress. Longer than vanilla's 15 s keep-alive timeout, which a client that
/// reads nothing for that long has already missed, so only dead sessions reach it.
const STALLED: Duration = Duration::from_secs(30);

pub(crate) async fn serve(shared: Arc<Shared>, client: TcpStream, peer: SocketAddr, permit: Permit) {
    if let Err(error) = handle(&shared, client, peer, permit).await {
        tracing::debug!(%peer, %error, "connection closed");
    }
}

async fn handle(shared: &Shared, mut client: TcpStream, peer: SocketAddr, permit: Permit) -> io::Result<()> {
    let opening = handshake::read(&mut client, shared.handshake_timeout).await?;
    let handshake = match opening.hello {
        Hello::Handshake(handshake) => handshake,
        Hello::LegacyPing { hostname: Some(hostname) } => return legacy_ping(shared, client, peer, &hostname).await,
        Hello::LegacyPing { hostname: None } => return Err(io::Error::other("a legacy ping that names no hostname")),
    };
    let entry = route(shared, &handshake.hostname)?;
    match handshake.intent {
        Intent::Status => {
            let local = client.local_addr()?;
            let mut client = Frames::new(&mut client, &opening.bytes[opening.length..]);
            status::request(&mut client, shared.handshake_timeout).await?;
            let answer = status_of(shared, entry, &handshake, peer, local).await;
            status::respond(&mut client, &answer, shared.handshake_timeout).await
        }
        Intent::Login | Intent::Transfer => login(shared, client, peer, permit, &entry, opening.bytes).await,
    }
}

fn route(shared: &Shared, hostname: &str) -> io::Result<Entry> {
    shared.routes.get(hostname).ok_or_else(|| io::Error::other(format!("no route for {hostname:?}")))
}

/// The status to answer: live while the environment is awake, and otherwise the one it reported, unless its pings wake
/// it and it wakes in time.
async fn status_of(
    shared: &Shared,
    mut entry: Entry,
    handshake: &Handshake,
    peer: SocketAddr,
    local: SocketAddr,
) -> Arc<str> {
    let mut gateways = entry.gateways();
    if gateways.is_empty() && entry.route.asleep && entry.route.sleeping_ping() == SleepingPingMode::Wake {
        let deadline = Instant::now() + shared.wake_timeout;
        let woken = wake::wake(&shared.management, &shared.routes, &entry.route, WakeReason::Ping, peer.ip(), deadline);
        if let Ok(woken) = woken.await {
            gateways = woken.gateways;
            entry = shared.routes.get(&handshake.hostname).unwrap_or(entry);
        }
    }
    if gateways.is_empty() {
        return status::cached(&entry.route, handshake.protocol).into();
    }
    let fetch = Fetch {
        gateways: &gateways,
        peer,
        local,
        hostname: &handshake.hostname,
        protocol: handshake.protocol,
        port: handshake.port,
    };
    status::live(&entry.status, &fetch).await
}

/// Splices a login to a gateway with the player's address and every byte it sent, waking the environment first if it
/// has no gateway. A login the environment isn't woken for is disconnected with the reason, and one it is woken for
/// refunds a wake that counted toward the wake limit once it completes.
async fn login(
    shared: &Shared,
    mut client: TcpStream,
    peer: SocketAddr,
    permit: Permit,
    entry: &Entry,
    mut sent: Vec<u8>,
) -> io::Result<()> {
    let mut gateways = entry.gateways();
    let mut refund_token = None;
    if gateways.is_empty() {
        let deadline = Instant::now() + shared.wake_timeout;
        let woken = tokio::select! {
            woken = wake::wake(&shared.management, &shared.routes, &entry.route, WakeReason::Login, peer.ip(), deadline) => woken,
            error = hold(&mut client, &mut sent) => return Err(error),
        };
        match woken {
            Ok(woken) => (gateways, refund_token) = (woken.gateways, woken.refund_token),
            Err(refusal) => {
                tracing::debug!(%peer, hostname = entry.route.hostname, ?refusal, "login not woken for");
                client.write_all(&wire::disconnect(refusal.message())?).await?;
                return client.shutdown().await;
            }
        }
    }
    let mut gateway = gateway::connect(&gateways, peer).await?;
    let mut preamble = proxy_header::v2(peer, client.local_addr()?);
    preamble.extend_from_slice(&sent);
    gateway.write_all(&preamble).await?;
    drop(permit);
    _ = client.set_nodelay(true);
    let relayed = relay(&mut client, &mut gateway, shared.handshake_timeout, STALLED);
    let Some(refund_token) = refund_token else { return relayed.await };
    // Still spliced past the gateway's login deadline, the login completed, so its wake no longer counts.
    tokio::pin!(relayed);
    tokio::select! {
        closed = &mut relayed => return closed,
        () = sleep(shared.login_timeout) => wake::refund(&shared.management, entry.route.environment_id.clone(), refund_token),
    }
    relayed.await
}

/// Copies both ways, passing on each side's close, until one side closes; the other direction then has `closing` to
/// finish, so a peer that never closes can't keep the relay open. A write that makes no progress for `stalled` ends its
/// direction too, since a side that stops reading hides the other's close behind the data queued for it.
async fn relay(
    client: &mut TcpStream,
    gateway: &mut TcpStream,
    closing: Duration,
    stalled: Duration,
) -> io::Result<()> {
    let (mut client_read, mut client_write) = client.split();
    let (mut gateway_read, mut gateway_write) = gateway.split();
    let upstream = pipe(&mut client_read, &mut gateway_write, stalled);
    let downstream = pipe(&mut gateway_read, &mut client_write, stalled);
    tokio::pin!(upstream, downstream);
    tokio::select! {
        closed = &mut upstream => {
            _ = timeout(closing, downstream).await;
            closed
        }
        closed = &mut downstream => {
            _ = timeout(closing, upstream).await;
            closed
        }
    }
}

async fn pipe(
    from: &mut (impl AsyncRead + Unpin),
    to: &mut (impl AsyncWrite + Unpin),
    stalled: Duration,
) -> io::Result<()> {
    let stall = || io::Error::new(io::ErrorKind::TimedOut, "the other side stopped reading");
    let mut buffer = vec![0; RELAY_BUFFER];
    loop {
        let read = from.read(&mut buffer).await?;
        if read == 0 {
            return timeout(stalled, to.shutdown()).await.map_err(|_| stall())?;
        }
        let mut written = 0;
        while written < read {
            match timeout(stalled, to.write(&buffer[written..read])).await.map_err(|_| stall())?? {
                0 => return Err(io::ErrorKind::WriteZero.into()),
                count => written += count,
            }
        }
    }
}

/// Keeps what a held login sends, for replay, until it closes or sends too much.
async fn hold(client: &mut TcpStream, sent: &mut Vec<u8>) -> io::Error {
    loop {
        if sent.len() > MAX_HELD {
            return invalid("too much sent while held");
        }
        match client.read_buf(sent).await {
            Ok(0) => return io::ErrorKind::UnexpectedEof.into(),
            Ok(_) => {}
            Err(error) => return error,
        }
    }
}

/// Answers a 1.6 ping for `hostname`, from a live status while its environment is awake. It never wakes one.
async fn legacy_ping(shared: &Shared, mut client: TcpStream, peer: SocketAddr, hostname: &str) -> io::Result<()> {
    let entry = route(shared, hostname)?;
    let gateways = entry.gateways();
    let answer = if gateways.is_empty() {
        status::cached(&entry.route, 0).into()
    } else {
        let local = client.local_addr()?;
        let fetch = Fetch { gateways: &gateways, peer, local, hostname, protocol: 0, port: 25565 };
        status::live(&entry.status, &fetch).await
    };
    client.write_all(&status::legacy(&answer)).await?;
    client.shutdown().await
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;

    use super::*;

    async fn pair(listener: &TcpListener) -> (TcpStream, TcpStream) {
        let (connected, accepted) = tokio::join!(TcpStream::connect(listener.local_addr().unwrap()), listener.accept());
        (connected.unwrap(), accepted.unwrap().0)
    }

    #[tokio::test]
    async fn ends_a_relay_whose_gateway_closed_while_the_client_stays_open_and_reads_nothing() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        // Nothing, then more than the socket buffers on the way hold.
        for queued in [0, 32 << 20] {
            let (mut player, mut client) = pair(&listener).await;
            let (mut gateway, mut to_gateway) = pair(&listener).await;
            let sending = tokio::spawn(async move {
                _ = gateway.write_all(&vec![0; queued]).await;
            });
            let closing = Duration::from_millis(50);
            let relayed = timeout(Duration::from_secs(5), relay(&mut client, &mut to_gateway, closing, closing * 2));
            let result = relayed.await.expect("the relay ended");
            drop((client, to_gateway));
            sending.await.unwrap();
            if queued == 0 {
                result.unwrap();
                assert_eq!(player.read(&mut [0]).await.unwrap(), 0, "the player saw the close");
            }
        }
    }
}
