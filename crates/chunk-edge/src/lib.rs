//! The edge: accepts players' Minecraft connections and hands each to a gateway of the environment its handshake's
//! hostname routes to, with the player's address in a PROXY protocol v2 header. Routes come from management's
//! `WatchRoutes`; unknown hostnames are closed. The edge answers server-list pings itself, and wakes a sleeping
//! environment for a login through management's `Wake`.

mod connection;
mod gateway;
mod handshake;
mod health;
mod limits;
mod proxy_header;
mod routes;
mod status;
mod wake;
mod wire;

use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use chunk_management::Client;
use chunk_service::{optional, required};
use tokio::{net::TcpListener, task::JoinSet};
use tokio_util::sync::CancellationToken;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Under the 30 s vanilla clients wait for a login or status response.
const WAKE_TIMEOUT: Duration = Duration::from_secs(25);
/// Connections not yet spliced to a gateway, in all and from one client.
const MAX_PENDING: usize = 8192;
const MAX_PENDING_PER_CLIENT: usize = 32;

pub struct Config {
    /// Where players connect.
    pub bind: SocketAddr,
    /// Where to serve `GET /ready`; no health listener when unset.
    pub health_bind: Option<SocketAddr>,
    /// The management service routes come from.
    pub management_url: String,
    /// The edge token management's `EdgeService` accepts.
    pub edge_token: String,
    /// How long a client has, from connecting, to send its handshake, and then for each step of a status exchange; also
    /// how long a relay's other side has to close once one side has.
    pub handshake_timeout: Duration,
    /// How long a login, or a ping that wakes its environment, is held while the environment wakes.
    pub wake_timeout: Duration,
}

impl Config {
    /// Reads `CHUNK_BIND` (default `0.0.0.0:25565`), `CHUNK_HEALTH_BIND` (default unset), `CHUNK_MANAGEMENT_URL`,
    /// `CHUNK_EDGE_TOKEN`, `CHUNK_HANDSHAKE_TIMEOUT_MS` (default 5000) and `CHUNK_WAKE_TIMEOUT_MS` (default 25000).
    /// # Errors
    /// Reports missing or invalid variables.
    pub fn from_env() -> io::Result<Self> {
        Ok(Self {
            bind: optional("CHUNK_BIND")?.unwrap_or(([0, 0, 0, 0], 25565).into()),
            health_bind: optional("CHUNK_HEALTH_BIND")?,
            management_url: required("CHUNK_MANAGEMENT_URL")?,
            edge_token: required("CHUNK_EDGE_TOKEN")?,
            handshake_timeout: milliseconds("CHUNK_HANDSHAKE_TIMEOUT_MS", HANDSHAKE_TIMEOUT)?,
            wake_timeout: milliseconds("CHUNK_WAKE_TIMEOUT_MS", WAKE_TIMEOUT)?,
        })
    }
}

fn milliseconds(name: &str, default: Duration) -> io::Result<Duration> {
    let duration = optional(name)?.map_or(default, Duration::from_millis);
    if duration.is_zero() {
        return Err(io::Error::other(format!("{name} must be positive")));
    }
    Ok(duration)
}

/// What every connection shares.
struct Shared {
    routes: routes::Routes,
    management: Client,
    handshake_timeout: Duration,
    wake_timeout: Duration,
}

/// The player listener, and the health listener if configured.
pub struct Edge {
    listener: TcpListener,
    health: Option<TcpListener>,
    config: Config,
}

impl Edge {
    /// # Errors
    /// Reports bind errors.
    pub async fn bind(config: Config) -> io::Result<Self> {
        let listener = TcpListener::bind(config.bind).await?;
        tracing::info!(address = %listener.local_addr()?, "edge listening");
        let health = match config.health_bind {
            Some(address) => {
                let health = TcpListener::bind(address).await?;
                tracing::info!(address = %health.local_addr()?, "edge serving readiness");
                Some(health)
            }
            None => None,
        };
        Ok(Self { listener, health, config })
    }

    /// The health listener's address, if configured.
    /// # Errors
    /// Reports a listener whose address cannot be read.
    pub fn health_addr(&self) -> io::Result<Option<SocketAddr>> {
        self.health.as_ref().map(TcpListener::local_addr).transpose()
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
        let management = Client::new(&self.config.management_url).with_token(&self.config.edge_token);
        let watcher = tokio::spawn(routes::watch(management.clone(), routes.clone()));
        let health = self.health.map(|listener| tokio::spawn(health::serve(listener, routes.clone())));
        let shared = Arc::new(Shared {
            routes,
            management,
            handshake_timeout: self.config.handshake_timeout,
            wake_timeout: self.config.wake_timeout,
        });
        let limits = limits::Limits::new(MAX_PENDING, MAX_PENDING_PER_CLIENT);
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                () = stop.cancelled() => break,
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, peer)) => {
                        if let Some(permit) = limits.admit(peer.ip()) {
                            connections.spawn(connection::serve(shared.clone(), stream, peer, permit));
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "accept failed; retrying");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
            }
        }
        watcher.abort();
        if let Some(health) = health {
            health.abort();
        }
        connections.shutdown().await;
        Ok(())
    }
}
