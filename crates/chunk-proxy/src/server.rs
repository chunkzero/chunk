mod authentication;
mod configuration;
mod connection;
mod gameplay;
mod limbo;
mod managed;
mod platform;
mod transport;

#[cfg(feature = "bench-support")]
pub use managed::benchmark;
#[cfg(feature = "test-support")]
pub use managed::testing;

use authentication::Authentication;

use std::{
    future::Future,
    io,
    net::SocketAddr,
    sync::{Arc, PoisonError, RwLock},
    time::Duration,
};

use chunk_protocol::{
    McString, encode_packet,
    versions::SUPPORTED,
    versions::v26_2::{LoginDisconnect, StatusResponse},
};
use tokio::{
    net::TcpListener,
    task::JoinSet,
    time::{Instant, sleep_until},
};

use crate::{Config, PlatformTarget};

struct Responses {
    status: Vec<u8>,
    unsupported_version: Vec<u8>,
}

impl Responses {
    fn new(config: &Config) -> io::Result<Self> {
        let status = status_packet(&config.motd, 0, 0)?;
        let supported_names = SUPPORTED.iter().map(|version| version.name).collect::<Vec<_>>().join(", ");
        let unsupported_version = encode_packet(&LoginDisconnect {
            reason: McString::new(
                serde_json::json!({
                    "text": format!("Unsupported Minecraft version. This edge supports {supported_names}.")
                })
                .to_string(),
            )
            .map_err(invalid_config)?,
        })
        .map_err(invalid_config)?;
        Ok(Self { status, unsupported_version })
    }
}

/// A status response advertising the newest supported version.
fn status_packet(motd: &str, online: u32, max: u32) -> io::Result<Vec<u8>> {
    let version = SUPPORTED.last().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "no Minecraft version enabled; enable a version feature")
    })?;
    let json = serde_json::json!({
        "version": { "name": version.name, "protocol": version.protocol },
        "players": { "max": max, "online": online },
        "description": { "text": motd },
    });
    encode_packet(&StatusResponse { json: McString::new(json.to_string()).map_err(invalid_config)? })
        .map_err(invalid_config)
}

fn invalid_config(error: chunk_protocol::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error)
}

pub struct Proxy {
    listener: TcpListener,
    config: Config,
    responses: Arc<Responses>,
    authentication: Arc<Authentication>,
    limbo_packets: Arc<limbo::Cache>,
    platform: Option<Arc<RwLock<platform::Platform>>>,
}

/// Replaces the managed platform for later connections; established connections keep theirs.
#[derive(Clone)]
pub struct Retarget(Arc<RwLock<platform::Platform>>);

impl Retarget {
    /// Routes later connections through `target`'s deployment, keeping the current core client and gateway identity.
    pub fn replace(&self, target: PlatformTarget) {
        let mut platform = self.0.write().unwrap_or_else(PoisonError::into_inner);
        *platform = platform.retarget(target);
    }

    /// The platform later connections use.
    fn platform(&self) -> platform::Platform {
        self.0.read().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl Proxy {
    /// Validates responses before binding the listener.
    ///
    /// # Errors
    /// Returns configuration validation or socket binding errors.
    pub async fn bind(address: SocketAddr, config: Config) -> io::Result<Self> {
        if config.connection_timeout.is_zero() || config.configuration_timeout.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "connection and configuration timeouts must be positive",
            ));
        }
        if config.compression_threshold.is_some_and(|threshold| threshold > chunk_protocol::MAX_FRAME_SIZE) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "compression threshold exceeds frame limit"));
        }
        let platform =
            config.platform.clone().map(platform::Platform::new).transpose()?.map(|p| Arc::new(RwLock::new(p)));
        let responses = Arc::new(Responses::new(&config)?);
        if config.offline_logins {
            tracing::warn!(
                "OFFLINE LOGINS ENABLED: players are not authenticated and can join under any name; for local testing only"
            );
        }
        let authentication = Arc::new(Authentication::new(config.offline_logins).await?);
        let limbo_packets = Arc::new(limbo::Cache::new(config.compression_threshold)?);
        let listener = TcpListener::bind(address).await?;
        tracing::info!(address = %listener.local_addr()?, "Minecraft listener ready");
        Ok(Self { listener, config, responses, authentication, limbo_packets, platform })
    }

    /// # Errors
    /// Returns the underlying socket error if its address cannot be read.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// A handle for changing the managed platform, when one is configured.
    #[must_use]
    pub fn retarget(&self) -> Option<Retarget> {
        self.platform.clone().map(Retarget)
    }

    /// Serves until shutdown, then closes all player sockets and joins tasks. With a managed platform, it first
    /// withdraws the claims an earlier process under this gateway's ID left open, and accepts no connection until then.
    /// It stops once a later process under that ID replaces it.
    ///
    /// # Errors
    /// Returns shutdown-signal errors and replacement by a later process. Accept errors are retried with backoff.
    /// Client failures are logged and do not stop the listener.
    pub async fn run(self, shutdown: impl Future<Output = io::Result<()>>) -> io::Result<()> {
        tokio::pin!(shutdown);
        let platform = self.retarget().as_ref().map(Retarget::platform);
        let replaced = async {
            match &platform {
                Some(platform) => platform.replaced().await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(replaced);
        if let Some(platform) = &platform {
            tokio::select! {
                biased;
                result = &mut shutdown => return result,
                error = &mut replaced => return Err(error),
                withdrawn = managed::withdraw_inherited(platform) => withdrawn?,
            }
        }
        let mut connections = JoinSet::new();
        let mut accept_after = Instant::now();
        let result = loop {
            tokio::select! {
                biased;
                result = &mut shutdown => break result,
                error = &mut replaced => break Err(error),
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    if let Err(error) = result {
                        tracing::error!(%error, "connection task failed");
                    }
                }
                accepted = async {
                    sleep_until(accept_after).await;
                    self.listener.accept().await
                } => {
                    let (stream, peer) = match accepted {
                        Ok(accepted) => accepted,
                        Err(error) => {
                            tracing::warn!(%error, "accept failed; retrying");
                            accept_after = Instant::now() + Duration::from_millis(100);
                            continue;
                        }
                    };
                    if connections.len() >= self.config.max_connections.get() {
                        drop(stream);
                        continue;
                    }
                    // Relay writes are already batched; small packets should not wait for ACKs.
                    if let Err(error) = stream.set_nodelay(true) {
                        tracing::debug!(%peer, %error, "could not disable Nagle's algorithm");
                    }
                    let responses = Arc::clone(&self.responses);
                    let authentication = Arc::clone(&self.authentication);
                    let limbo_packets = Arc::clone(&self.limbo_packets);
                    let deadline = self.config.connection_timeout;
                    let compression = self.config.compression_threshold;
                    let configuration_timeout = self.config.configuration_timeout;
                    let current = self.retarget();
                    let platform = current.as_ref().map(Retarget::platform);
                    connections.spawn(async move {
                        match connection::serve(stream, &responses, &authentication, deadline, compression, platform.as_ref()).await {
                            Ok(Some(authenticated)) => {
                                if let Some(current) = current {
                                    if let Err(error) = Box::pin(managed::serve(authenticated, &current, configuration_timeout)).await {
                                        tracing::debug!(%peer, %error, "managed connection closed");
                                    }
                                    return;
                                }
                                if let Err(error) = route(authenticated, &limbo_packets, configuration_timeout).await {
                                    tracing::debug!(%peer, %error, "player connection closed");
                                }
                            }
                            Ok(None) => {}
                            Err(error) => tracing::debug!(%peer, %error, "connection closed"),
                        }
                    });
                }
            }
        };
        drop(self.listener);
        connections.shutdown().await;
        if let Some(platform) = self.platform {
            let platform = platform.read().unwrap_or_else(PoisonError::into_inner).clone();
            platform.cleanup.close();
            platform.cleanup.wait().await;
        }
        result
    }
}

async fn route(
    authenticated: authentication::Authenticated<tokio::net::TcpStream>,
    limbo_packets: &limbo::Cache,
    deadline: Duration,
) -> io::Result<()> {
    limbo::wait_for_destination(authenticated, std::future::pending::<io::Result<()>>(), deadline, limbo_packets)
        .await
        .map(|_| ())
}
