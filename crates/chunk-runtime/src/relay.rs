use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use chunk_proto::v1::{PlayerDelivery, PlayerPreparation, PlayerSetup};
use chunk_protocol::{
    McString, RemainingBytes, VarInt,
    versions::v26_1::{Handshake, LoginPluginRequest, LoginPluginResponse, LoginStart, LoginSuccess},
};
use prost::Message;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{OnceCell, Semaphore},
    task::JoinSet,
    time::timeout,
};

use crate::{
    service::{Registered, Shared},
    wire,
};

pub(crate) struct Binding {
    pub delivery: PlayerDelivery,
    pub registered: Registered,
    pub downstream: OnceCell<PlayerPreparation>,
    capability: Vec<u8>,
    created: Instant,
    consumed: AtomicBool,
    pub closed: AtomicBool,
}

impl Binding {
    pub fn new(delivery: PlayerDelivery, registered: Registered) -> Self {
        Self {
            delivery,
            registered,
            downstream: OnceCell::new(),
            capability: [
                uuid::Uuid::new_v4().as_bytes().as_slice(),
                uuid::Uuid::new_v4().as_bytes().as_slice(),
            ]
            .concat(),
            created: Instant::now(),
            consumed: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        }
    }

    pub fn result(&self, shared: &Shared) -> PlayerPreparation {
        PlayerPreparation {
            operation_id: self.delivery.operation_id.clone(),
            endpoint: shared.ingress.to_string(),
            capability: self.capability.clone(),
        }
    }
}

struct CloseBinding(Arc<Binding>);
impl Drop for CloseBinding {
    fn drop(&mut self) {
        self.0.closed.store(true, Ordering::Release);
    }
}

pub(crate) async fn accept(listener: TcpListener, shared: Arc<Shared>) -> io::Result<()> {
    let slots = Arc::new(Semaphore::new(128));
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            () = shared.shutdown.cancelled() => break,
            result = tasks.join_next(), if !tasks.is_empty() => { let _ = result; },
            accepted = listener.accept() => {
                let (socket, _) = accepted?;
                if let Ok(permit) = slots.clone().try_acquire_owned() {
                    let shared = shared.clone();
                    tasks.spawn(async move {
                        let _permit = permit;
                        if let Err(error) = serve(socket, shared).await {
                            tracing::debug!(%error, "player relay closed");
                        }
                    });
                }
            }
        }
    }
    tasks.shutdown().await;
    Ok(())
}

async fn serve(mut upstream: TcpStream, shared: Arc<Shared>) -> io::Result<()> {
    let mut downstream = timeout(Duration::from_secs(5), admit(&mut upstream, &shared)).await??;
    let (mut from_proxy, mut to_proxy) = upstream.split();
    let (mut from_jvm, mut to_jvm) = downstream.0.split();
    tokio::select! {
        result = pump(&mut from_proxy, &mut to_jvm) => result,
        result = pump(&mut from_jvm, &mut to_proxy) => result,
    }
}

async fn admit(upstream: &mut TcpStream, shared: &Shared) -> io::Result<(TcpStream, CloseBinding)> {
    upstream.set_nodelay(true)?;
    let handshake: Handshake = wire::read_packet(upstream).await?;
    let start: LoginStart = wire::read_packet(upstream).await?;
    if handshake.next_state != VarInt(2) {
        return Err(io::Error::other("expected login"));
    }
    wire::write_packet(
        upstream,
        &LoginPluginRequest {
            message_id: VarInt(0),
            channel: McString::new("chunk:delivery").map_err(io::Error::other)?,
            data: RemainingBytes::new(vec![]).map_err(io::Error::other)?,
        },
    )
    .await?;
    let response: LoginPluginResponse = wire::read_packet(upstream).await?;
    if response.message_id != VarInt(0) {
        return Err(io::Error::other("unexpected login response"));
    }
    let payload = response.data.ok_or_else(|| io::Error::other("missing capability"))?;
    if payload.as_slice().len() > 4096 {
        return Err(io::Error::other("oversized capability"));
    }
    let setup = PlayerSetup::decode(payload.as_slice()).map_err(io::Error::other)?;
    let binding = shared
        .bindings
        .lock()
        .map_err(|_| io::Error::other("bindings poisoned"))?
        .get(&setup.operation_id)
        .cloned()
        .ok_or_else(|| io::Error::other("unknown delivery"))?;
    if setup.capability != binding.capability
        || binding.created.elapsed() > Duration::from_secs(30)
        || binding.closed.load(Ordering::Acquire)
        || binding.consumed.swap(true, Ordering::AcqRel)
    {
        return Err(io::Error::other("invalid delivery setup"));
    }
    let close = CloseBinding(binding.clone());
    let prepared = binding
        .downstream
        .get()
        .ok_or_else(|| io::Error::other("unprepared delivery"))?;
    let mut downstream = timeout(Duration::from_secs(5), TcpStream::connect(&prepared.endpoint)).await??;
    downstream.set_nodelay(true)?;
    wire::write_packet(&mut downstream, &handshake).await?;
    wire::write_packet(&mut downstream, &start).await?;
    let challenge: LoginPluginRequest = wire::read_packet(&mut downstream).await?;
    if challenge.channel.as_str() != "chunk:delivery" {
        return Err(io::Error::other("unexpected JVM challenge"));
    }
    wire::write_packet(
        &mut downstream,
        &LoginPluginResponse {
            message_id: challenge.message_id,
            data: Some(
                RemainingBytes::new(
                    PlayerSetup {
                        operation_id: prepared.operation_id.clone(),
                        capability: prepared.capability.clone(),
                    }
                    .encode_to_vec(),
                )
                .map_err(io::Error::other)?,
            ),
        },
    )
    .await?;
    let success: LoginSuccess = wire::read_packet(&mut downstream).await?;
    wire::write_packet(upstream, &success).await?;
    Ok((downstream, close))
}

async fn pump<R: tokio::io::AsyncRead + Unpin, W: tokio::io::AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
) -> io::Result<()> {
    let mut bytes = Box::new([0; 8192]);
    loop {
        let count = reader.read(bytes.as_mut()).await?;
        if count == 0 {
            return Ok(());
        }
        timeout(Duration::from_secs(5), writer.write_all(&bytes[..count])).await??;
    }
}
