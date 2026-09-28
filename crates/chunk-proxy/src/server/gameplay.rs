use std::{io, net::SocketAddr};

use chunk_proto::sync::v1::{PlayerIdentity, PlayerProperty, PlayerSetup};
use chunk_protocol::{
    McString, RemainingBytes, VarInt, decode_packet,
    versions::v26_2::{
        ConfigurationClientInformation, Handshake, LoginAcknowledged, LoginPluginRequest, LoginPluginResponse,
        LoginStart, LoginSuccess,
    },
};
use prost::Message;
use tokio::net::TcpStream;

use super::{
    authentication::Authenticated,
    configuration,
    transport::{Transport, invalid_data},
};

pub(super) fn identity(profile: &LoginSuccess) -> PlayerIdentity {
    PlayerIdentity {
        uuid: uuid::Uuid::from_bytes(profile.uuid.0).to_string(),
        username: profile.username.as_str().into(),
        properties: profile
            .properties
            .as_slice()
            .iter()
            .map(|p| PlayerProperty {
                name: p.name.as_str().into(),
                value: p.value.as_str().into(),
                signature: p.signature.as_ref().map(|s| s.as_str().into()),
            })
            .collect(),
    }
}

pub(super) async fn login<S>(
    authenticated: &Authenticated<S>,
    settings: &ConfigurationClientInformation,
    endpoint: &str,
    setup: PlayerSetup,
) -> io::Result<Transport<TcpStream>> {
    if setup.capability.len() != 32 {
        return Err(invalid_data("invalid player preparation"));
    }
    let address = destination(endpoint)?;
    let socket = TcpStream::connect(address).await?;
    socket.set_nodelay(true)?;
    let mut internal = Transport::new(socket);
    internal
        .write_packet(&Handshake {
            protocol_version: VarInt(authenticated.protocol_version),
            server_address: McString::new("localhost").map_err(invalid_data)?,
            server_port: address.port(),
            next_state: VarInt(2),
        })
        .await?;
    internal
        .write_packet(&LoginStart {
            username: authenticated.profile.username.clone(),
            player_uuid: authenticated.profile.uuid,
        })
        .await?;
    let challenge = decode_packet::<LoginPluginRequest>(&internal.read_frame(4096).await?).map_err(invalid_data)?;
    if challenge.channel.as_str() != "chunk:delivery" {
        return Err(invalid_data("unexpected login plugin request"));
    }
    internal
        .write_packet(&LoginPluginResponse {
            message_id: challenge.message_id,
            data: Some(RemainingBytes::new(setup.encode_to_vec()).map_err(invalid_data)?),
        })
        .await?;
    let profile =
        decode_packet::<LoginSuccess>(&internal.read_frame(configuration::FRAME_LIMIT).await?).map_err(invalid_data)?;
    if profile.uuid != authenticated.profile.uuid
        || profile.username != authenticated.profile.username
        || profile.properties != authenticated.profile.properties
    {
        return Err(invalid_data("destination login identity mismatch"));
    }
    internal.write_packet(&LoginAcknowledged).await?;
    internal.write_packet(settings).await?;
    Ok(internal)
}

/// The JVM's player listener, which must be a private address.
fn destination(endpoint: &str) -> io::Result<SocketAddr> {
    let address: SocketAddr = endpoint.parse().map_err(invalid_data)?;
    if !chunk_service::net::private(address.ip()) {
        return Err(invalid_data("gameplay endpoint must be a private address"));
    }
    Ok(address)
}

#[cfg(test)]
mod tests;
