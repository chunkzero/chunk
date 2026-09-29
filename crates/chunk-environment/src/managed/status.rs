//! Core's status reports to management, which route players to the gateway addresses they carry.

use super::{Interrupted, REQUEST_TIMEOUT, deadline};
use chunk_management::{Client, v1};
use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime},
};
use tokio::{
    sync::{Mutex, OnceCell},
    time::{Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

/// How often core reports while nothing changes.
pub(super) const INTERVAL: Duration = Duration::from_secs(15);
/// How often core looks for a change to report at once.
pub(super) const OBSERVE: Duration = Duration::from_secs(1);

/// What a report says besides deployment progress.
#[derive(Clone, Default)]
pub(super) struct Observed {
    /// The lease of the attach the report is under.
    pub lease: u64,
    /// The desired revision the report reflects.
    pub revision: u64,
    /// Where edges reach this core's gateway, as `ip:port`.
    pub gateway_addresses: Vec<String>,
    pub online_players: u32,
}

/// Sends reports one at a time, so each lease's sequence reaches management in order.
pub(super) struct Reporter {
    client: Client,
    /// Held while a report is sent.
    sent: Mutex<Sent>,
    /// Cancelled once core shuts down, after which no report starts.
    stopping: CancellationToken,
}

/// The latest report.
#[derive(Default)]
struct Sent {
    lease: u64,
    sequence: u64,
    gateway_addresses: Vec<String>,
    at: Option<Instant>,
}

impl Reporter {
    pub(super) fn new(client: Client, stopping: CancellationToken) -> Self {
        Self { client, sent: Mutex::default(), stopping }
    }

    /// Reports `observed` and `deployment`'s progress under the next sequence of `observed.lease`, which starts over
    /// with each lease. Returns whether it reported, which it doesn't once core is stopping.
    pub(super) async fn send(
        &self,
        observed: Observed,
        deployment: Option<v1::DeploymentProgress>,
    ) -> Result<bool, Interrupted> {
        let mut sent = self.sent.lock().await;
        if self.stopping.is_cancelled() {
            return Ok(false);
        }
        if observed.lease > sent.lease {
            *sent = Sent { lease: observed.lease, ..Sent::default() };
        }
        sent.sequence += 1;
        sent.gateway_addresses.clone_from(&observed.gateway_addresses);
        sent.at = Some(Instant::now());
        let request = v1::ReportStatusRequest {
            observe_time: Some(SystemTime::now().into()),
            gateway_addresses: observed.gateway_addresses,
            online_players: observed.online_players,
            pings: Vec::new(),
            deployment,
            ready_to_suspend: false,
            lease: observed.lease,
            sequence: sent.sequence,
            desired_revision: observed.revision,
        };
        deadline(REQUEST_TIMEOUT, self.client.report_status(&request)).await?;
        Ok(true)
    }

    /// Reports what `observe` finds every [`INTERVAL`], and within [`OBSERVE`] once its gateway addresses differ from
    /// the latest report's. `observe` finds nothing while core must not report. Returns once management fences a
    /// lease `superseded` says another core superseded.
    pub(super) async fn keep_reporting<F>(&self, observe: impl Fn() -> F, superseded: impl Fn(u64) -> bool) -> io::Error
    where
        F: Future<Output = Option<Observed>>,
    {
        let mut tick = tokio::time::interval(OBSERVE);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(observed) = observe().await else { continue };
            let due = {
                let sent = self.sent.lock().await;
                sent.gateway_addresses != observed.gateway_addresses
                    || sent.at.is_none_or(|at| at.elapsed() >= INTERVAL)
            };
            if !due {
                continue;
            }
            let lease = observed.lease;
            match self.send(observed, None).await {
                Ok(_) => {}
                Err(Interrupted::Fenced(error)) if superseded(lease) => return error,
                Err(Interrupted::Retry(error) | Interrupted::Fatal(error) | Interrupted::Fenced(error)) => {
                    tracing::warn!(%error, "status report failed");
                }
            }
        }
    }
}

/// This machine's private address, where edges reach its gateway: the configured one, or else the local address of a
/// route towards management.
pub(super) struct PrivateAddress {
    configured: Option<IpAddr>,
    management_url: String,
    management: OnceCell<SocketAddr>,
    warned: AtomicBool,
}

impl PrivateAddress {
    pub(super) fn new(configured: Option<IpAddr>, management_url: &str) -> Self {
        Self {
            configured,
            management_url: management_url.to_owned(),
            management: OnceCell::new(),
            warned: AtomicBool::new(false),
        }
    }

    /// Where edges reach a gateway listening on `bound`. One listening on every interface is reached at this
    /// machine's private address, if it accepts that address's family: `0.0.0.0` accepts IPv4 only, and `[::]` is
    /// taken to accept IPv6 only, since whether it also accepts IPv4 depends on the host. Why no such address is found
    /// is warned about once.
    pub(super) async fn gateway(&self, bound: SocketAddr) -> Option<SocketAddr> {
        let listener = bound.ip().to_canonical();
        if !listener.is_unspecified() {
            return Some(SocketAddr::new(listener, bound.port()));
        }
        let found = match self.configured {
            Some(address) => Ok(address),
            None => self.route().await,
        };
        let problem = match found.map(|address| address.to_canonical()) {
            Ok(address) if address.is_ipv4() != listener.is_ipv4() => {
                format!("the gateway listening on {listener} does not accept {address}")
            }
            Ok(address) if chunk_service::net::private(address) => return Some(SocketAddr::new(address, bound.port())),
            Ok(address) => format!("{address} is not private"),
            Err(error) => error.to_string(),
        };
        if !self.warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                problem,
                "this machine's private address is unknown, so no gateway is reported; set CHUNK_PRIVATE_ADDRESS"
            );
        }
        None
    }

    /// The local address of a UDP socket connected towards management, which sends nothing.
    async fn route(&self) -> io::Result<IpAddr> {
        let management = *self.management.get_or_try_init(|| resolve(&self.management_url)).await?;
        let unspecified = match management {
            SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        };
        let socket = UdpSocket::bind((unspecified, 0))?;
        socket.connect(management)?;
        Ok(socket.local_addr()?.ip())
    }
}

async fn resolve(url: &str) -> io::Result<SocketAddr> {
    let url = url::Url::parse(url).map_err(io::Error::other)?;
    let port = url.port_or_known_default().ok_or_else(|| io::Error::other("the management URL names no port"))?;
    match url.host().ok_or_else(|| io::Error::other("the management URL names no host"))? {
        url::Host::Ipv4(address) => Ok((address, port).into()),
        url::Host::Ipv6(address) => Ok((address, port).into()),
        url::Host::Domain(domain) => tokio::net::lookup_host((domain, port))
            .await?
            .next()
            .ok_or_else(|| io::Error::other(format!("{domain} resolves to no address"))),
    }
}
