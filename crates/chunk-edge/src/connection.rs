//! One player's connection: routed by its handshake, then either answered with a status or spliced to a gateway,
//! waking a sleeping environment first.

use std::{io, net::SocketAddr, sync::Arc};

use chunk_management::v1::{SleepingPingMode, WakeReason};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::Instant,
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
        if let Ok(ready) = woken.await {
            gateways = ready;
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
/// has no gateway. A login the environment isn't woken for is disconnected with the reason.
async fn login(
    shared: &Shared,
    mut client: TcpStream,
    peer: SocketAddr,
    permit: Permit,
    entry: &Entry,
    mut sent: Vec<u8>,
) -> io::Result<()> {
    let mut gateways = entry.gateways();
    if gateways.is_empty() {
        let deadline = Instant::now() + shared.wake_timeout;
        let woken = tokio::select! {
            woken = wake::wake(&shared.management, &shared.routes, &entry.route, WakeReason::Login, peer.ip(), deadline) => woken,
            error = hold(&mut client, &mut sent) => return Err(error),
        };
        match woken {
            Ok(ready) => gateways = ready,
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
    tokio::io::copy_bidirectional(&mut client, &mut gateway).await?;
    Ok(())
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
