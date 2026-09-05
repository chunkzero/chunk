use std::{future::Future, io, time::Duration};

use chunk_protocol::{
    Decode, Packet, VarInt, decode_packet,
    versions::v26_1::{
        ConfigurationClientInformation, ConfigurationKeepAlive, ConfigurationKeepAliveResponse,
        ConfigurationPluginResponse,
    },
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    time::{Instant, sleep_until, timeout},
};

use super::{authentication::Authenticated, transport::invalid_data};

const FRAME_LIMIT: usize = 65536;
const CLIENT_INFORMATION_TIMEOUT: Duration = Duration::from_secs(10);
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(10);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Keeps the client in configuration until a destination is ready. The caller
/// receives the authenticated transport and latest client settings, with no
/// outstanding keepalive, and owns the remaining configuration exchange.
/// Cancelling this future drops the connection and destination future.
pub(super) async fn wait_for_destination<S, T>(
    mut authenticated: Authenticated<S>,
    destination: impl Future<Output = io::Result<T>>,
    max_wait: Duration,
) -> io::Result<(Authenticated<S>, ConfigurationClientInformation, T)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::pin!(destination);
    let started = Instant::now();
    let expires = started + max_wait;
    let information_deadline = started + CLIENT_INFORMATION_TIMEOUT;
    let mut information = None;
    let mut ready = None;
    let mut next_keep_alive = started;
    let mut pending_keep_alive = None;
    let mut next_id = 0_i64;

    loop {
        if pending_keep_alive.is_none()
            && let Some(settings) = information.take()
        {
            if let Some(destination) = ready.take() {
                return Ok((authenticated, settings, destination));
            }
            information = Some(settings);
        }
        let heartbeat_deadline = pending_keep_alive.map_or(next_keep_alive, |(_, deadline)| deadline);
        tokio::select! {
            biased;
            () = sleep_until(expires) => return Err(timed_out("configuration wait expired")),
            () = sleep_until(information_deadline), if information.is_none() => {
                return Err(timed_out("client information timed out"));
            }
            result = &mut destination, if ready.is_none() => ready = Some(result?),
            () = sleep_until(heartbeat_deadline) => {
                if pending_keep_alive.is_some() {
                    return Err(timed_out("configuration keepalive timed out"));
                }
                let id = next_id;
                next_id = next_id.wrapping_add(1);
                // Never cancel a partially written encrypted packet and then reuse the stream.
                timeout(WRITE_TIMEOUT.min(expires.saturating_duration_since(Instant::now())), authenticated.transport.write_packet(
                    &ConfigurationKeepAlive { keep_alive_id: id },
                )).await.map_err(|_| timed_out("configuration write timed out"))??;
                pending_keep_alive = Some((id, Instant::now() + KEEP_ALIVE_TIMEOUT));
            }
            frame = authenticated.transport.read_frame(FRAME_LIMIT) => {
                let frame = frame?;
                match VarInt::decode(&mut frame.as_ref()).map_err(invalid_data)?.0 {
                    ConfigurationClientInformation::ID => {
                        information = Some(decode_packet::<ConfigurationClientInformation>(&frame).map_err(invalid_data)?);
                    }
                    ConfigurationPluginResponse::ID => {
                        // Brand and channel announcements do not need a response while parked.
                        decode_packet::<ConfigurationPluginResponse>(&frame).map_err(invalid_data)?;
                    }
                    ConfigurationKeepAliveResponse::ID => {
                        let response = decode_packet::<ConfigurationKeepAliveResponse>(&frame).map_err(invalid_data)?;
                        if pending_keep_alive.is_none_or(|(id, _)| id != response.keep_alive_id) {
                            return Err(invalid_data("unexpected configuration keepalive response"));
                        }
                        pending_keep_alive = None;
                        next_keep_alive = Instant::now() + KEEP_ALIVE_INTERVAL;
                    }
                    _ => return Err(invalid_data("unexpected packet while waiting in configuration")),
                }
            }
        }
    }
}

fn timed_out(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, message)
}

#[cfg(test)]
#[path = "configuration_tests.rs"]
mod tests;
