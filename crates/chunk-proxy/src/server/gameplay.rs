use std::{io, time::Duration};

use chunk_proto::v1::{
    ConfigurationRequest, ConfigurationResponse, DeploymentRef, Identity, PlayerActivation, PlayerDelivery, PlayerRef,
    PlayerSetup, Property, SessionRef, gameplay_client::GameplayClient,
};
use chunk_protocol::{
    BoundedArray, Decode, Encode, Packet, VarInt, decode_packet,
    versions::v26_1::{
        AcknowledgeConfiguration, ConfigurationClientInformation, ConfigurationPluginResponse, FinishConfiguration,
        KnownPacks, SelectKnownPacks,
    },
};
use prost::Message;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    time::timeout,
};
use tonic::{Request, transport::Channel};

use super::{
    authentication::Authenticated,
    configuration,
    transport::{Transport, invalid_data},
};
use crate::GameplayTarget;

const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const INPUT_LIMIT: usize = 65_536;

fn request<T>(body: T, target: &GameplayTarget) -> io::Result<Request<T>> {
    let mut request = Request::new(body);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", target.token).parse().map_err(invalid_data)?,
    );
    Ok(request)
}

async fn destination(target: &GameplayTarget) -> io::Result<(GameplayClient<Channel>, ConfigurationResponse)> {
    let channel = Channel::from_shared(target.endpoint.clone())
        .map_err(invalid_data)?
        .connect_timeout(Duration::from_secs(5))
        .connect()
        .await
        .map_err(io::Error::other)?;
    let mut client = GameplayClient::new(channel).max_decoding_message_size(8 * 1024 * 1024);
    let deployment = DeploymentRef {
        environment: target.environment.clone(),
        deployment: target.deployment.clone(),
    };
    let configuration = client
        .configuration(request(
            ConfigurationRequest {
                deployment: Some(deployment.clone()),
            },
            target,
        )?)
        .await
        .map_err(io::Error::other)?
        .into_inner();
    if configuration.deployment != Some(deployment)
        || configuration.process_generation == 0
        || configuration.registry_digest.len() != 32
        || configuration.packets.is_empty()
    {
        return Err(invalid_data("invalid gameplay configuration identity"));
    }
    let mut digest = openssl::sha::Sha256::new();
    for frame in &configuration.packets {
        if frame.packet.is_empty() || frame.packet.len() > chunk_protocol::MAX_FRAME_SIZE {
            return Err(invalid_data("invalid configuration packet size"));
        }
        digest.update(&frame.packet);
    }
    if digest.finish().as_slice() != configuration.registry_digest {
        return Err(invalid_data("destination registry digest does not match packets"));
    }
    Ok((client, configuration))
}

pub(super) async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    authenticated: Authenticated<S>,
    target: &GameplayTarget,
    deadline: Duration,
) -> io::Result<()> {
    let (mut authenticated, mut settings, (mut client, config)) =
        configuration::wait_for_destination(authenticated, destination(target), deadline).await?;
    if config.protocol != authenticated.protocol_version {
        return Err(invalid_data("destination protocol differs from authenticated client"));
    }
    let mut client_information = Vec::new();
    settings.encode(&mut client_information).map_err(invalid_data)?;
    let identity = &authenticated.profile;
    let uuid = uuid::Uuid::from_bytes(identity.uuid.0).to_string();
    let delivery = PlayerDelivery {
        deployment: config.deployment.clone(),
        process_generation: config.process_generation,
        operation_id: uuid::Uuid::new_v4().to_string(),
        session: Some(SessionRef { id: "bridge".into() }),
        player: Some(PlayerRef { id: uuid.clone() }),
        owner_generation: u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos(),
        )
        .map_err(invalid_data)?,
        identity: Some(Identity {
            uuid,
            username: identity.username.as_str().into(),
            properties: identity
                .properties
                .as_slice()
                .iter()
                .map(|p| Property {
                    name: p.name.as_str().into(),
                    value: p.value.as_str().into(),
                    signature: p.signature.as_ref().map(|s| s.as_str().into()),
                })
                .collect(),
        }),
        protocol: authenticated.protocol_version,
        registry_digest: config.registry_digest.clone(),
        client_information,
    };
    let operation_id = delivery.operation_id.clone();
    let mut internal = prepare(&mut client, delivery, target).await?;
    timeout(
        deadline,
        configure(&mut authenticated.transport, &mut settings, &config),
    )
    .await
    .map_err(io::Error::other)??;
    let mut client_information = Vec::new();
    settings.encode(&mut client_information).map_err(invalid_data)?;
    timeout(
        WRITE_TIMEOUT,
        client.activate_player(request(
            PlayerActivation {
                operation_id,
                client_information,
            },
            target,
        )?),
    )
    .await
    .map_err(io::Error::other)?
    .map_err(io::Error::other)?;
    tracing::info!("authenticated player delivered over TCP");
    loop {
        tokio::select! {
            frame = authenticated.transport.read_frame(INPUT_LIMIT) => {
                timeout(WRITE_TIMEOUT, internal.write_body(&frame?)).await.map_err(io::Error::other)??;
            }
            frame = internal.read_frame(chunk_protocol::MAX_FRAME_SIZE) => {
                timeout(WRITE_TIMEOUT, authenticated.transport.write_body(&frame?)).await.map_err(io::Error::other)??;
            }
        }
    }
}

async fn prepare(
    client: &mut GameplayClient<Channel>,
    delivery: PlayerDelivery,
    target: &GameplayTarget,
) -> io::Result<Transport<TcpStream>> {
    let delivery_operation = delivery.operation_id.clone();
    let prepared = timeout(WRITE_TIMEOUT, client.prepare_player(request(delivery, target)?))
        .await
        .map_err(io::Error::other)?
        .map_err(io::Error::other)?
        .into_inner();
    if prepared.operation_id != delivery_operation || prepared.capability.len() != 32 {
        return Err(invalid_data("invalid player preparation"));
    }
    let address: std::net::SocketAddr = prepared.endpoint.parse().map_err(invalid_data)?;
    if !address.ip().is_loopback() {
        return Err(invalid_data("local gameplay endpoint must be loopback"));
    }
    let socket = timeout(WRITE_TIMEOUT, TcpStream::connect(address))
        .await
        .map_err(io::Error::other)??;
    socket.set_nodelay(true)?;
    let mut internal = Transport::new(socket);
    let setup = PlayerSetup {
        operation_id: delivery_operation.clone(),
        capability: prepared.capability,
    };
    timeout(WRITE_TIMEOUT, internal.write_body(&setup.encode_to_vec()))
        .await
        .map_err(io::Error::other)??;
    // The setup reply is a single byte, before framed gameplay begins.
    if timeout(WRITE_TIMEOUT, internal.read_setup_ack())
        .await
        .map_err(io::Error::other)??
        != 0
    {
        return Err(invalid_data("player preparation rejected"));
    }
    Ok(internal)
}

async fn configure<S: AsyncRead + AsyncWrite + Unpin>(
    transport: &mut Transport<S>,
    settings: &mut ConfigurationClientInformation,
    config: &ConfigurationResponse,
) -> io::Result<()> {
    transport
        .write_packet(&SelectKnownPacks {
            packs: BoundedArray::new(vec![]).map_err(invalid_data)?,
        })
        .await?;
    loop {
        let frame = transport.read_frame(INPUT_LIMIT).await?;
        if packet_id(&frame)? == KnownPacks::ID {
            if !decode_packet::<KnownPacks>(&frame)
                .map_err(invalid_data)?
                .packs
                .as_slice()
                .is_empty()
            {
                return Err(invalid_data("client selected an unoffered pack"));
            }
            break;
        }
        configuration_message(&frame, settings)?;
    }
    for frame in &config.packets {
        transport.write_body(&frame.packet).await?;
    }
    transport.write_packet(&FinishConfiguration).await?;
    loop {
        let frame = transport.read_frame(INPUT_LIMIT).await?;
        if packet_id(&frame)? == AcknowledgeConfiguration::ID {
            decode_packet::<AcknowledgeConfiguration>(&frame).map_err(invalid_data)?;
            return Ok(());
        }
        configuration_message(&frame, settings)?;
    }
}

fn packet_id(mut frame: &[u8]) -> io::Result<i32> {
    Ok(VarInt::decode(&mut frame).map_err(invalid_data)?.0)
}

fn configuration_message(frame: &[u8], settings: &mut ConfigurationClientInformation) -> io::Result<()> {
    match packet_id(frame)? {
        ConfigurationClientInformation::ID => *settings = decode_packet(frame).map_err(invalid_data)?,
        ConfigurationPluginResponse::ID => {
            decode_packet::<ConfigurationPluginResponse>(frame).map_err(invalid_data)?;
        }
        _ => return Err(invalid_data("unexpected destination configuration packet")),
    }
    Ok(())
}
