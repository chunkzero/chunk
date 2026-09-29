//! The edge: accepts players' Minecraft connections and hands each to a gateway of the environment its handshake's
//! hostname routes to, with the player's address in a PROXY protocol v2 header. Routes come from management's
//! `WatchRoutes`; unknown hostnames are closed.

mod connection;
mod handshake;
mod proxy_header;
mod routes;

use std::{io, net::SocketAddr, time::Duration};

use chunk_service::{optional, required};
use tokio::{net::TcpListener, task::JoinSet};
use tokio_util::sync::CancellationToken;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Config {
    /// Where players connect.
    pub bind: SocketAddr,
    /// The management service routes come from.
    pub management_url: String,
    /// The edge token management's `EdgeService` accepts.
    pub edge_token: String,
    /// How long a client has, from connecting, to send its handshake.
    pub handshake_timeout: Duration,
}

impl Config {
    /// Reads `CHUNK_BIND` (default `0.0.0.0:25565`), `CHUNK_MANAGEMENT_URL`, `CHUNK_EDGE_TOKEN` and
    /// `CHUNK_HANDSHAKE_TIMEOUT_MS` (default 5000).
    /// # Errors
    /// Reports missing or invalid variables.
    pub fn from_env() -> io::Result<Self> {
        let handshake_timeout =
            optional("CHUNK_HANDSHAKE_TIMEOUT_MS")?.map_or(HANDSHAKE_TIMEOUT, Duration::from_millis);
        if handshake_timeout.is_zero() {
            return Err(io::Error::other("CHUNK_HANDSHAKE_TIMEOUT_MS must be positive"));
        }
        Ok(Self {
            bind: optional("CHUNK_BIND")?.unwrap_or(([0, 0, 0, 0], 25565).into()),
            management_url: required("CHUNK_MANAGEMENT_URL")?,
            edge_token: required("CHUNK_EDGE_TOKEN")?,
            handshake_timeout,
        })
    }
}

/// The player listener.
pub struct Edge {
    listener: TcpListener,
    config: Config,
}

impl Edge {
    /// # Errors
    /// Reports bind errors.
    pub async fn bind(config: Config) -> io::Result<Self> {
        let listener = TcpListener::bind(config.bind).await?;
        tracing::info!(address = %listener.local_addr()?, "edge listening");
        Ok(Self { listener, config })
    }

    /// # Errors
    /// Reports a listener whose address cannot be read.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Follows management's routes and serves players until `stop`, then closes every connection.
    /// # Errors
    /// None: accept failures are retried, and a failed connection closes only itself.
    pub async fn run(self, stop: CancellationToken) -> io::Result<()> {
        let routes = routes::Routes::default();
        let client = chunk_management::Client::new(&self.config.management_url).with_token(&self.config.edge_token);
        let watcher = tokio::spawn(routes::watch(client, routes.clone()));
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                () = stop.cancelled() => break,
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, peer)) => {
                        connections.spawn(connection::serve(stream, peer, routes.clone(), self.config.handshake_timeout));
                    }
                    Err(error) => {
                        tracing::warn!(%error, "accept failed; retrying");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
            }
        }
        watcher.abort();
        connections.shutdown().await;
        Ok(())
    }
}
