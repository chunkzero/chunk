#[path = "authentication.rs"]
mod authentication;
#[path = "configuration.rs"]
mod configuration;
#[path = "connection.rs"]
mod connection;
#[path = "limbo/mod.rs"]
mod limbo;
#[path = "transport.rs"]
mod transport;

use authentication::Authentication;

use std::{future::Future, io, net::SocketAddr, sync::Arc, time::Duration};

use chunk_protocol::{
    McString, encode_packet,
    versions::SUPPORTED,
    versions::v26_1::{LoginDisconnect, StatusResponse},
};
use tokio::{
    net::TcpListener,
    task::JoinSet,
    time::{Instant, sleep_until},
};

use crate::Config;

struct Responses {
    status: Vec<u8>,
    unsupported_version: Vec<u8>,
}

impl Responses {
    fn new(config: &Config) -> io::Result<Self> {
        let version = SUPPORTED.last().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "no Minecraft version enabled; enable a version feature",
            )
        })?;
        let json = serde_json::json!({
            "version": { "name": version.name, "protocol": version.protocol },
            "players": { "max": 0, "online": 0 },
            "description": { "text": config.motd },
        });
        let status = encode_packet(&StatusResponse {
            json: McString::new(json.to_string()).map_err(invalid_config)?,
        })
        .map_err(invalid_config)?;
        let supported_names = SUPPORTED
            .iter()
            .map(|version| version.name)
            .collect::<Vec<_>>()
            .join(", ");
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
        Ok(Self {
            status,
            unsupported_version,
        })
    }
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
        if config
            .compression_threshold
            .is_some_and(|threshold| threshold > chunk_protocol::MAX_FRAME_SIZE)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "compression threshold exceeds frame limit",
            ));
        }
        let responses = Arc::new(Responses::new(&config)?);
        let authentication = Arc::new(Authentication::new().await?);
        let limbo_packets = Arc::new(limbo::Cache::new(config.compression_threshold)?);
        let listener = TcpListener::bind(address).await?;
        tracing::info!(address = %listener.local_addr()?, "Minecraft listener ready");
        Ok(Self {
            listener,
            config,
            responses,
            authentication,
            limbo_packets,
        })
    }

    /// # Errors
    /// Returns the underlying socket error if its address cannot be read.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serves until shutdown, then closes all player sockets and joins tasks.
    ///
    /// # Errors
    /// Returns shutdown-signal errors. Accept errors are retried with backoff.
    /// Client failures are logged and do not stop the listener.
    pub async fn run(self, shutdown: impl Future<Output = io::Result<()>>) -> io::Result<()> {
        tokio::pin!(shutdown);
        let mut connections = JoinSet::new();
        let mut accept_after = Instant::now();
        let result = loop {
            tokio::select! {
                biased;
                result = &mut shutdown => break result,
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
                    let responses = Arc::clone(&self.responses);
                    let authentication = Arc::clone(&self.authentication);
                    let limbo_packets = Arc::clone(&self.limbo_packets);
                    let deadline = self.config.connection_timeout;
                    let compression = self.config.compression_threshold;
                    let configuration_timeout = self.config.configuration_timeout;
                    connections.spawn(async move {
                        match connection::serve(stream, &responses, &authentication, deadline, compression).await {
                            Ok(Some(authenticated)) => {
                                tracing::info!(username = authenticated.profile.username.as_str(), "authenticated player reached configuration; entering limbo");
                                // Destination selection will supply this future when sessions are available.
                                if let Err(error) = limbo::wait_for_destination(
                                    authenticated, std::future::pending::<io::Result<()>>(), configuration_timeout, &limbo_packets,
                                ).await {
                                    tracing::debug!(%peer, %error, "limbo connection closed");
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
        result
    }
}
