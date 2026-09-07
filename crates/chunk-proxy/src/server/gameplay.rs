use std::{io, time::Duration};

use chunk_proto::v1::{
    ConfigurationRequest, ConfigurationResponse, DeploymentRef, Identity, PlayerDelivery, PlayerPreparation, PlayerRef,
    PlayerSetup, Property, SessionRef, gameplay_client::GameplayClient,
};
use chunk_protocol::{
    McString, RemainingBytes, VarInt, decode_packet,
    versions::v26_1::{
        ConfigurationClientInformation, Handshake, LoginAcknowledged, LoginPluginRequest, LoginPluginResponse,
        LoginStart, LoginSuccess,
    },
};
use prost::Message;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tonic::{Request, transport::Channel};

use super::{
    authentication::Authenticated,
    configuration,
    transport::{Transport, WRITE_TIMEOUT, invalid_data, within},
};
use crate::GameplayTarget;

struct Destination {
    client: GameplayClient<Channel>,
    target: GameplayTarget,
}

impl Destination {
    fn request<T>(&self, body: T) -> io::Result<Request<T>> {
        let mut request = Request::new(body);
        request.metadata_mut().insert(
            "authorization",
            format!("Bearer {}", self.target.token).parse().map_err(invalid_data)?,
        );
        Ok(request)
    }

    async fn prepare_player(&mut self, delivery: PlayerDelivery) -> io::Result<PlayerPreparation> {
        let request = self.request(delivery)?;
        within(WRITE_TIMEOUT, async {
            self.client
                .prepare_player(request)
                .await
                .map(tonic::Response::into_inner)
                .map_err(io::Error::other)
        })
        .await
    }
}

async fn destination(target: &GameplayTarget) -> io::Result<(Destination, ConfigurationResponse)> {
    let channel = Channel::from_shared(target.endpoint.clone())
        .map_err(invalid_data)?
        .connect_timeout(WRITE_TIMEOUT)
        .connect()
        .await
        .map_err(io::Error::other)?;
    let mut destination = Destination {
        client: GameplayClient::new(channel).max_decoding_message_size(65_536),
        target: target.clone(),
    };
    let deployment = DeploymentRef {
        environment: target.environment.clone(),
        deployment: target.deployment.clone(),
    };
    let request = destination.request(ConfigurationRequest {
        deployment: Some(deployment.clone()),
    })?;
    let configuration = within(WRITE_TIMEOUT, async {
        destination
            .client
            .configuration(request)
            .await
            .map(tonic::Response::into_inner)
            .map_err(io::Error::other)
    })
    .await?;
    if configuration.deployment != Some(deployment) || configuration.process_generation == 0 {
        return Err(invalid_data("invalid gameplay configuration identity"));
    }
    Ok((destination, configuration))
}

pub(super) fn identity(profile: &LoginSuccess) -> Identity {
    Identity {
        uuid: uuid::Uuid::from_bytes(profile.uuid.0).to_string(),
        username: profile.username.as_str().into(),
        properties: profile
            .properties
            .as_slice()
            .iter()
            .map(|p| Property {
                name: p.name.as_str().into(),
                value: p.value.as_str().into(),
                signature: p.signature.as_ref().map(|s| s.as_str().into()),
            })
            .collect(),
    }
}

fn delivery<S>(authenticated: &Authenticated<S>, config: &ConfigurationResponse) -> io::Result<PlayerDelivery> {
    let identity = identity(&authenticated.profile);
    Ok(PlayerDelivery {
        deployment: config.deployment.clone(),
        process_generation: config.process_generation,
        operation_id: uuid::Uuid::new_v4().to_string(),
        session: Some(SessionRef { id: "bridge".into() }),
        player: Some(PlayerRef {
            id: identity.uuid.clone(),
        }),
        // Fixture-only ownership until the control plane supplies placement generations.
        owner_generation: u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos(),
        )
        .map_err(invalid_data)?,
        identity: Some(identity),
        protocol: authenticated.protocol_version,
        runtime_id: config.runtime_id.clone(),
        session_generation: 1,
        membership_generation: 1,
        proxy_id: "bridge-fixture".into(),
        connection_id: uuid::Uuid::new_v4().to_string(),
    })
}

pub(super) async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    authenticated: Authenticated<S>,
    target: &GameplayTarget,
    deadline: Duration,
) -> io::Result<()> {
    let (mut authenticated, mut settings, (mut destination, config)) =
        configuration::wait_for_destination(authenticated, destination(target), deadline).await?;
    if config.protocol != authenticated.protocol_version {
        return Err(invalid_data("destination protocol differs from authenticated client"));
    }
    let delivery = delivery(&authenticated, &config)?;
    let operation = delivery.operation_id.clone();
    let prepared = destination.prepare_player(delivery).await?;
    if prepared.operation_id != operation || prepared.capability.len() != 32 {
        return Err(invalid_data("invalid player preparation"));
    }
    let mut internal = within(deadline, login(&authenticated, &settings, prepared)).await?;
    within(
        deadline,
        Box::pin(configuration::relay(
            &mut authenticated.transport,
            &mut internal,
            &mut settings,
        )),
    )
    .await?;
    tracing::info!("authenticated player admitted to Minestom listener");
    // Minestom owns the normal configuration and play exchange on this socket.
    loop {
        tokio::select! {
            frame = authenticated.transport.read_frame(configuration::FRAME_LIMIT) => {
                within(WRITE_TIMEOUT, internal.write_body(&frame?)).await?;
            }
            frame = internal.read_frame(chunk_protocol::MAX_FRAME_SIZE) => {
                within(WRITE_TIMEOUT, authenticated.transport.write_body(&frame?)).await?;
            }
        }
    }
}

pub(super) async fn login<S>(
    authenticated: &Authenticated<S>,
    settings: &ConfigurationClientInformation,
    prepared: PlayerPreparation,
) -> io::Result<Transport<TcpStream>> {
    if prepared.capability.len() != 32 {
        return Err(invalid_data("invalid player preparation"));
    }
    let address: std::net::SocketAddr = prepared.endpoint.parse().map_err(invalid_data)?;
    if !address.ip().is_loopback() {
        return Err(invalid_data("local gameplay endpoint must be loopback"));
    }
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
    let setup = PlayerSetup {
        operation_id: prepared.operation_id,
        capability: prepared.capability,
    };
    internal
        .write_packet(&LoginPluginResponse {
            message_id: challenge.message_id,
            data: Some(RemainingBytes::new(setup.encode_to_vec()).map_err(invalid_data)?),
        })
        .await?;
    let profile =
        decode_packet::<LoginSuccess>(&internal.read_frame(configuration::FRAME_LIMIT).await?).map_err(invalid_data)?;
    if profile != authenticated.profile {
        return Err(invalid_data("destination login identity mismatch"));
    }
    internal.write_packet(&LoginAcknowledged).await?;
    internal.write_packet(settings).await?;
    Ok(internal)
}

#[cfg(test)]
mod tests;
