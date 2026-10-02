//! Server-list status, which the edge always answers itself: fetched from an awake environment's gateway at most once
//! per [`FRESH`] per route, so ping floods don't reach it, or taken from the status the environment reported before it
//! slept, with 0 online.

use std::{
    io,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use chunk_management::v1::Route;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    sync::Mutex,
    time::timeout,
};

use crate::{
    gateway, proxy_header,
    wire::{self, Frames, invalid},
};

/// How long a status fetched from a gateway is reused.
const FRESH: Duration = Duration::from_secs(5);
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Room for a status response at the string limit, in three-byte characters.
const MAX_RESPONSE: usize = 128 * 1024;
const SLEEPING: &str = "This server is sleeping. Join to wake it up.";
const STARTING: &str = "This server is starting.";
const UNAVAILABLE: &str = "Server temporarily unavailable";

/// The status last fetched for one route, and when.
#[derive(Default)]
pub(crate) struct Slot(Mutex<Option<(Instant, Arc<str>)>>);

/// Where a live status is fetched from, as whom.
pub(crate) struct Fetch<'a> {
    pub gateways: &'a [SocketAddr],
    pub peer: SocketAddr,
    pub local: SocketAddr,
    pub hostname: &'a str,
    pub protocol: i32,
    pub port: u16,
}

/// The status of an awake environment, fetched through a gateway unless `slot` holds one at most [`FRESH`] old.
/// Concurrent callers share one fetch. A failed fetch is answered, and cached, as unavailable.
pub(crate) async fn live(slot: &Slot, fetch: &Fetch<'_>) -> Arc<str> {
    let mut cached = slot.0.lock().await;
    if let Some((at, status)) = cached.as_ref()
        && at.elapsed() < FRESH
    {
        return status.clone();
    }
    let status: Arc<str> = match timeout(FETCH_TIMEOUT, fetch_status(fetch)).await {
        Ok(Ok(status)) => status.into(),
        failed => {
            let error = failed.map_or_else(|_| "timed out".into(), |result| result.unwrap_err().to_string());
            tracing::debug!(hostname = fetch.hostname, error, "status fetch failed");
            fallback(UNAVAILABLE, fetch.protocol).into()
        }
    };
    *cached = Some((Instant::now(), status.clone()));
    status
}

async fn fetch_status(fetch: &Fetch<'_>) -> io::Result<String> {
    let mut gateway = gateway::connect(fetch.gateways, fetch.peer).await?;
    let mut request = proxy_header::v2(fetch.peer, fetch.local);
    request.extend(wire::handshake(fetch.protocol, fetch.hostname, fetch.port, 1)?);
    request.extend(wire::frame(&[0])?);
    gateway.write_all(&request).await?;
    let response = Frames::new(&mut gateway, &[]).next(MAX_RESPONSE).await?;
    wire::read_string_packet(0, &response)
}

/// The status `route`'s environment reported for its hostname, with 0 online, or a placeholder if it reported none.
pub(crate) fn cached(route: &Route, protocol: i32) -> String {
    let reported = serde_json::from_str::<Value>(&route.cached_status_json).ok().and_then(|mut status| {
        let players = status.as_object_mut()?.entry("players").or_insert_with(|| json!({ "max": 0 }));
        let players = players.as_object_mut()?;
        players.insert("online".into(), 0.into());
        players.remove("sample");
        Some(status.to_string())
    });
    reported.unwrap_or_else(|| fallback(if route.asleep { SLEEPING } else { STARTING }, protocol))
}

/// A status showing `message`, compatible with the client's `protocol`.
fn fallback(message: &str, protocol: i32) -> String {
    json!({
        "version": { "name": "chunk", "protocol": protocol },
        "players": { "max": 0, "online": 0 },
        "description": { "text": message },
    })
    .to_string()
}

/// Reads a client's status request.
pub(crate) async fn request<S: AsyncRead + Unpin>(client: &mut Frames<S>, within: Duration) -> io::Result<()> {
    let request = timeout(within, client.next(1)).await.map_err(|_| timed_out("no status request in time"))??;
    if request[..] != [0] {
        return Err(invalid("not a status request"));
    }
    Ok(())
}

/// Answers a client's status request with `status`, then its ping if it sends one within `within`.
pub(crate) async fn respond<S: AsyncRead + AsyncWrite + Unpin>(
    client: &mut Frames<S>,
    status: &str,
    within: Duration,
) -> io::Result<()> {
    client.stream.write_all(&wire::string_packet(0, status)?).await?;
    let ping = match timeout(within, client.next(9)).await {
        Ok(Ok(ping)) => ping,
        Ok(Err(error)) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
        Ok(Err(error)) => return Err(error),
        Err(_) => return Err(timed_out("no ping in time")),
    };
    if ping.len() != 9 || ping[0] != 1 {
        return Err(invalid("not a ping"));
    }
    client.stream.write_all(&wire::frame(&ping)?).await?;
    client.stream.shutdown().await
}

/// The kick packet that answers a pre-1.7 ping with `status`, in the format 1.4 to 1.6 clients read.
pub(crate) fn legacy(status: &str) -> Vec<u8> {
    let status: Value = serde_json::from_str(status).unwrap_or_default();
    let field = |pointer| status.pointer(pointer).and_then(Value::as_u64).unwrap_or_default().to_string();
    let version = status.pointer("/version/name").and_then(Value::as_str).unwrap_or_default();
    let motd = text(status.get("description"));
    // 127 is a protocol no legacy client speaks, so the client shows the version name.
    let reply = ["§1", "127", version, &motd, &field("/players/online"), &field("/players/max")]
        .map(|part| part.replace('\0', ""))
        .join("\0");
    let units: Vec<u16> = reply.encode_utf16().take(usize::from(u16::MAX)).collect();
    let mut packet = vec![0xff];
    packet.extend_from_slice(&u16::try_from(units.len()).unwrap_or(u16::MAX).to_be_bytes());
    packet.extend(units.iter().flat_map(|unit| unit.to_be_bytes()));
    packet
}

/// The plain text of a chat component.
fn text(component: Option<&Value>) -> String {
    match component {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts.iter().map(|part| text(Some(part))).collect(),
        Some(Value::Object(component)) => {
            let extra = component.get("extra").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
            text(component.get("text")) + &extra.iter().map(|part| text(Some(part))).collect::<String>()
        }
        _ => String::new(),
    }
}

fn timed_out(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_a_sleeping_route_with_its_reported_status_and_no_one_online() {
        let reported = json!({
            "version": { "name": "1.21", "protocol": 776 },
            "players": { "max": 20, "online": 3, "sample": [{ "name": "Alex", "id": "0" }] },
            "description": { "text": "Lobby" },
            "enforcesSecureChat": false,
        });
        let route = Route { asleep: true, cached_status_json: reported.to_string(), ..Default::default() };
        let answered: Value = serde_json::from_str(&cached(&route, 5)).unwrap();
        assert_eq!(answered["players"], json!({ "max": 20, "online": 0 }));
        assert_eq!(answered["description"], reported["description"]);
        assert_eq!(answered["enforcesSecureChat"], false);

        let unreported: Value =
            serde_json::from_str(&cached(&Route { asleep: true, ..Default::default() }, 5)).unwrap();
        assert_eq!(unreported["version"]["protocol"], 5, "compatible with the client");
        assert_eq!(unreported["description"]["text"], SLEEPING);
    }

    #[test]
    fn answers_legacy_pings_in_their_own_format() {
        let status = json!({
            "version": { "name": "1.21" },
            "players": { "max": 20, "online": 0 },
            "description": { "text": "Lobby", "extra": [{ "text": " one" }] },
        });
        let packet = legacy(&status.to_string());
        let expected = "§1\u{0}127\u{0}1.21\u{0}Lobby one\u{0}0\u{0}20";
        let units: Vec<u16> = expected.encode_utf16().collect();
        assert_eq!(packet[..3], [0xff, 0, u8::try_from(units.len()).unwrap()]);
        assert_eq!(packet[3..], units.iter().flat_map(|unit| unit.to_be_bytes()).collect::<Vec<_>>());
    }
}
