#[path = "connection.rs"]
mod connection;

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
    disconnect: Vec<u8>,
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
        let disconnect = encode_packet(&LoginDisconnect {
            reason: McString::new(serde_json::json!({ "text": config.login_rejection }).to_string())
                .map_err(invalid_config)?,
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
            disconnect,
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
}

impl Proxy {
    /// Validates responses before binding the listener.
    ///
    /// # Errors
    /// Returns configuration validation or socket binding errors.
    pub async fn bind(address: SocketAddr, config: Config) -> io::Result<Self> {
        if config.connection_timeout.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "connection timeout must be positive",
            ));
        }
        let responses = Arc::new(Responses::new(&config)?);
        let listener = TcpListener::bind(address).await?;
        tracing::info!(address = %listener.local_addr()?, "Minecraft listener ready");
        Ok(Self {
            listener,
            config,
            responses,
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
                    let deadline = self.config.connection_timeout;
                    connections.spawn(async move {
                        if let Err(error) = connection::serve(stream, &responses, deadline).await {
                            tracing::debug!(%peer, %error, "connection closed");
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
