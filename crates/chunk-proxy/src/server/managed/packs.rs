//! The resource packs a managed connection's client holds across its sessions.

use std::{
    collections::{HashMap, HashSet},
    io,
    time::Duration,
};

use chunk_proto::sync::v1::ResourcePack;
use chunk_protocol::{
    Encode, McString, Packet, Uuid,
    commands::PlainText,
    decode_packet, encode_packet,
    resource_packs::AddResourcePack,
    versions::v26_2::{
        ConfigurationClientInformation, ConfigurationKeepAlive, ConfigurationKeepAliveResponse,
        ConfigurationPluginResponse, RemoveResourcePack, ResourcePackResponse,
    },
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    time::{Instant, sleep_until},
};

use super::super::{
    configuration::{self, FRAME_LIMIT, KeepAlive, packet_id},
    transport::{Transport, WRITE_TIMEOUT, invalid_data, timed_out, within},
};

const LOADED: i32 = 0;
/// Statuses a client reports before a pack's final one.
const ACCEPTED: i32 = 3;
const DOWNLOADED: i32 = 4;

/// The packs a client loaded, with the SHA-1 each loaded with, in the order it stacks them: later packs override
/// earlier ones.
#[derive(Default)]
pub(super) struct Packs(Vec<([u8; 16], String)>);

impl Packs {
    /// Brings a client in configuration to `wanted`, in its order: keeps the longest prefix of the stack it holds that
    /// `wanted` begins with, removes the rest, and sends the remaining wanted packs in order, waiting for each sent pack's
    /// final status while keeping the client alive. A required pack the client doesn't load disconnects it, as does
    /// `max_wait` passing while a required pack is pending; optional packs still pending then are withdrawn, and only a
    /// keepalive already sent is awaited. Returns with no keepalive outstanding.
    pub async fn apply<S: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        transport: &mut Transport<S>,
        settings: &mut ConfigurationClientInformation,
        wanted: &[ResourcePack],
        max_wait: Duration,
    ) -> io::Result<()> {
        let expires = Instant::now() + max_wait;
        let wanted = wanted
            .iter()
            .map(|pack| {
                let id: [u8; 16] =
                    pack.id.as_slice().try_into().map_err(|_| invalid_data("resource pack ID isn't a UUID"))?;
                Ok((id, pack))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let kept = self
            .0
            .iter()
            .zip(&wanted)
            .take_while(|((id, sha1), (wanted, pack))| id == wanted && *sha1 == pack.sha1)
            .count();
        for (id, _) in self.0.drain(kept..) {
            queue(transport, &RemoveResourcePack { uuid: Some(Uuid(id)) })?;
        }
        let sent = &wanted[kept..];
        for (id, pack) in sent {
            let prompt = (!pack.prompt.is_empty()).then(|| PlainText::new(pack.prompt.clone())).transpose();
            let add = AddResourcePack {
                uuid: Uuid(*id),
                url: McString::new(pack.url.clone()).map_err(invalid_data)?,
                hash: McString::new(pack.sha1.clone()).map_err(invalid_data)?,
                forced: pack.required,
                prompt: prompt.map_err(invalid_data)?,
            };
            queue(transport, &add)?;
        }
        within(WRITE_TIMEOUT, transport.flush()).await?;

        let mut pending: HashMap<_, _> = sent.iter().map(|(id, pack)| (*id, *pack)).collect();
        let mut loaded = HashSet::new();
        let mut keep_alive = KeepAlive::new();
        let mut withdrawn = false;
        while !pending.is_empty() || !keep_alive.idle() {
            tokio::select! {
                biased;
                () = sleep_until(expires), if !withdrawn => {
                    if pending.values().any(|pack| pack.required) {
                        let _ = configuration::disconnect(transport, 0x02, "Resource pack download timed out.").await;
                        return Err(timed_out("resource pack wait expired"));
                    }
                    for (id, pack) in pending.drain() {
                        tracing::info!(url = %pack.url, "client didn't load an optional resource pack in time");
                        queue(transport, &RemoveResourcePack { uuid: Some(Uuid(id)) })?;
                    }
                    within(WRITE_TIMEOUT, transport.flush()).await?;
                    withdrawn = true;
                }
                () = sleep_until(keep_alive.deadline()), if !withdrawn || !keep_alive.idle() => {
                    let id = keep_alive.start().ok_or_else(|| timed_out("configuration keepalive timed out"))?;
                    within(WRITE_TIMEOUT, transport.write_packet(&ConfigurationKeepAlive { keep_alive_id: id })).await?;
                }
                frame = transport.read_frame(FRAME_LIMIT) => {
                    let frame = frame?;
                    match packet_id(&frame)? {
                        ResourcePackResponse::ID => {
                            let response = decode_packet::<ResourcePackResponse>(&frame).map_err(invalid_data)?;
                            if matches!(response.result.0, ACCEPTED | DOWNLOADED) {
                                continue;
                            }
                            // Statuses of packs this exchange didn't send, or already withdrew, are ignored.
                            let Some(pack) = pending.remove(&response.uuid.0) else { continue };
                            if response.result.0 == LOADED {
                                loaded.insert(response.uuid.0);
                            } else if pack.required {
                                let reason = "This server requires its resource pack.";
                                let _ = configuration::disconnect(transport, 0x02, reason).await;
                                return Err(io::Error::new(io::ErrorKind::PermissionDenied, reason));
                            } else {
                                tracing::info!(url = %pack.url, status = response.result.0, "client didn't load an optional resource pack");
                            }
                        }
                        ConfigurationClientInformation::ID => {
                            *settings = decode_packet(&frame).map_err(invalid_data)?;
                        }
                        ConfigurationPluginResponse::ID => {
                            decode_packet::<ConfigurationPluginResponse>(&frame).map_err(invalid_data)?;
                        }
                        ConfigurationKeepAliveResponse::ID => {
                            let response = decode_packet::<ConfigurationKeepAliveResponse>(&frame).map_err(invalid_data)?;
                            if !keep_alive.acknowledge(response.keep_alive_id) {
                                return Err(invalid_data("unexpected configuration keepalive response"));
                            }
                        }
                        _ => return Err(invalid_data("unexpected packet while loading resource packs")),
                    }
                }
            }
        }
        self.0.extend(sent.iter().filter(|(id, _)| loaded.contains(id)).map(|(id, pack)| (*id, pack.sha1.clone())));
        Ok(())
    }
}

fn queue<S, P>(transport: &mut Transport<S>, packet: &P) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    P: Packet + Encode,
{
    transport.queue_encoded(&encode_packet(packet).map_err(invalid_data)?)
}

#[cfg(test)]
mod tests;
