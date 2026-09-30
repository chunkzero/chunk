//! Core's status reports to management, which route players to the gateway addresses they carry.

use super::{Interrupted, deadline};
use chunk_management::{Client, v1};
use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket},
    pin::Pin,
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};
use tokio::{
    sync::OnceCell,
    time::{Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

/// How often core reports while nothing changes.
pub(super) const INTERVAL: Duration = Duration::from_secs(15);
/// How often core looks for a change to report at once.
pub(super) const OBSERVE: Duration = Duration::from_secs(1);
/// How long one report may take before it counts as failed.
pub(super) const REPORT_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest wait before a failed report is sent again.
const RETRY_LIMIT: Duration = Duration::from_secs(4);

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
    pub ready_to_suspend: bool,
    /// The status the gateway last answered for each hostname, which edges answer pings with while core sleeps.
    pub pings: Vec<v1::PingStatus>,
}

impl Observed {
    fn said(&self) -> Said {
        Said {
            gateway_addresses: self.gateway_addresses.clone(),
            ready_to_suspend: self.ready_to_suspend,
            players_online: self.online_players > 0,
        }
    }
}

/// What a change is reported at once for.
#[derive(Clone, PartialEq, Eq)]
struct Said {
    gateway_addresses: Vec<String>,
    ready_to_suspend: bool,
    /// Whether any player is online, which management takes as proof that a login wake led to a login, so a player
    /// who leaves before the next heartbeat is still reported.
    players_online: bool,
}

/// Sends reports under increasing sequences, never waiting for one to finish before sending the next. Management
/// ignores a report once it took a later one, so the latest report started wins.
pub(super) struct Reporter {
    client: Client,
    /// Never held across a request.
    sent: Mutex<Sent>,
    /// Cancelled once core shuts down, after which no report starts.
    stopping: CancellationToken,
}

/// The reports under one lease.
#[derive(Default)]
struct Sent {
    lease: u64,
    /// The gateway addresses of the latest report management accepted, under this lease or an earlier one.
    addresses: Vec<String>,
    sequence: u64,
    /// What the latest report started said.
    latest: Option<Said>,
    /// The latest report management accepted.
    accepted: Option<Accepted>,
    /// Set once a report fails, whose outcome management may or may not have applied, until a later one is accepted.
    failed: Option<Failed>,
    /// The deployment progress of a report in flight, and its sequence. Every report started meanwhile carries it too,
    /// so one management takes after it can't drop it.
    progress: Option<(u64, v1::DeploymentProgress)>,
}

struct Accepted {
    sequence: u64,
    /// When it was sent.
    at: Instant,
}

struct Failed {
    sequence: u64,
    /// Failures in a row.
    count: u32,
    retry_at: Instant,
}

impl Sent {
    /// Whether `said` differs from the latest report started.
    fn changed(&self, said: &Said) -> bool {
        self.latest.as_ref() != Some(said)
    }

    /// Whether `said` is to be reported now: at once when it changed, once a failed report's retry is due, and
    /// otherwise once the accepted report is [`INTERVAL`] old.
    fn due(&self, said: &Said) -> bool {
        if self.changed(said) {
            return true;
        }
        if let Some(failed) = &self.failed {
            return Instant::now() >= failed.retry_at;
        }
        self.accepted.as_ref().is_none_or(|accepted| accepted.at.elapsed() >= INTERVAL)
    }

    /// Records how report `sequence`, sent `at`, ended: accepted with its gateway addresses, or not.
    fn finished(&mut self, sequence: u64, at: Instant, accepted: Option<Vec<String>>) {
        let later = self.accepted.as_ref().is_none_or(|accepted| sequence > accepted.sequence);
        if let Some(addresses) = accepted {
            if later {
                self.accepted = Some(Accepted { sequence, at });
                self.addresses = addresses;
            }
            if self.failed.as_ref().is_some_and(|failed| failed.sequence <= sequence) {
                self.failed = None;
            }
        } else if later {
            let count = self.failed.as_ref().map_or(1, |failed| failed.count.saturating_add(1));
            let wait = OBSERVE.saturating_mul(2_u32.saturating_pow(count - 1)).min(RETRY_LIMIT);
            self.failed = Some(Failed { sequence, count, retry_at: at + wait });
        }
    }
}

/// Stops carrying a report's deployment progress once that report ends, however it ends.
struct Carried<'a> {
    sent: &'a Mutex<Sent>,
    sequence: u64,
}

impl Drop for Carried<'_> {
    fn drop(&mut self) {
        let mut sent = lock(self.sent);
        if sent.progress.as_ref().is_some_and(|(sequence, _)| *sequence == self.sequence) {
            sent.progress = None;
        }
    }
}

type Report<'a> = Pin<Box<dyn Future<Output = Result<bool, Interrupted>> + Send + 'a>>;

impl Reporter {
    pub(super) fn new(client: Client, stopping: CancellationToken) -> Self {
        Self { client, sent: Mutex::default(), stopping }
    }

    /// Reports `observed` and `deployment`'s progress under the next sequence of `observed.lease`, which starts over
    /// with each lease, within [`REPORT_TIMEOUT`]. Without `deployment`, it carries the progress of a report still in
    /// flight. Returns whether it reported, which it doesn't once core is stopping.
    pub(super) async fn send(
        &self,
        observed: Observed,
        deployment: Option<v1::DeploymentProgress>,
    ) -> Result<bool, Interrupted> {
        let (request, _carried) = {
            let mut sent = lock(&self.sent);
            if self.stopping.is_cancelled() {
                return Ok(false);
            }
            if observed.lease > sent.lease {
                *sent =
                    Sent { lease: observed.lease, addresses: std::mem::take(&mut sent.addresses), ..Sent::default() };
            }
            sent.sequence += 1;
            let sequence = sent.sequence;
            let carried = deployment.as_ref().map(|progress| {
                sent.progress = Some((sequence, progress.clone()));
                Carried { sent: &self.sent, sequence }
            });
            if observed.lease == sent.lease {
                sent.latest = Some(observed.said());
            }
            let request = v1::ReportStatusRequest {
                observe_time: Some(SystemTime::now().into()),
                gateway_addresses: observed.gateway_addresses,
                online_players: observed.online_players,
                pings: observed.pings,
                deployment: deployment.or_else(|| sent.progress.as_ref().map(|(_, progress)| progress.clone())),
                ready_to_suspend: observed.ready_to_suspend,
                lease: observed.lease,
                sequence,
                desired_revision: observed.revision,
            };
            (request, carried)
        };
        let sent_at = Instant::now();
        let result = deadline(REPORT_TIMEOUT, self.client.report_status(&request)).await;
        let mut sent = lock(&self.sent);
        if request.lease == sent.lease {
            sent.finished(request.sequence, sent_at, result.is_ok().then_some(request.gateway_addresses));
        }
        result.map(|_| true)
    }

    /// The gateway addresses of the latest report management accepted.
    pub(super) fn accepted_addresses(&self) -> Vec<String> {
        lock(&self.sent).addresses.clone()
    }

    /// Reports what `observe` finds every [`INTERVAL`], and within [`OBSERVE`] once its gateway addresses, readiness
    /// to suspend or whether any player is online change. A change doesn't wait for a report in flight: it cancels that one and is sent under the next
    /// sequence. After a failed report, whose outcome is unknown, it reports again after a backoff of one to four
    /// seconds until one is accepted. `observe` finds nothing while core must not report. Observations run beside the
    /// report in flight, so a slow one never holds up that report's timeout. Returns once management fences a lease
    /// `superseded` says another core superseded.
    pub(super) async fn keep_reporting<F>(&self, observe: impl Fn() -> F, superseded: impl Fn(u64) -> bool) -> io::Error
    where
        F: Future<Output = Option<Observed>>,
    {
        let mut tick = tokio::time::interval(OBSERVE);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut in_flight: Option<(u64, Report<'_>)> = None;
        let mut observing: Option<Pin<Box<F>>> = None;
        loop {
            tokio::select! {
                result = async { in_flight.as_mut().expect("a report is in flight").1.as_mut().await },
                    if in_flight.is_some() =>
                {
                    let lease = in_flight.take().map_or(0, |(lease, _)| lease);
                    match result {
                        Ok(_) => {}
                        Err(Interrupted::Fenced(error)) if superseded(lease) => return error,
                        Err(Interrupted::Retry(error) | Interrupted::Fatal(error) | Interrupted::Fenced(error)) => {
                            tracing::warn!(%error, "status report failed");
                        }
                    }
                }
                observed = async { observing.as_mut().expect("an observation is running").as_mut().await },
                    if observing.is_some() =>
                {
                    observing = None;
                    let Some(observed) = observed else { continue };
                    let said = observed.said();
                    let (changed, due) = {
                        let sent = lock(&self.sent);
                        (sent.changed(&said), sent.due(&said))
                    };
                    if changed || (due && in_flight.is_none()) {
                        in_flight = Some((observed.lease, Box::pin(self.send(observed, None))));
                    }
                }
                _ = tick.tick(), if observing.is_none() => observing = Some(Box::pin(observe())),
            }
        }
    }
}

fn lock(sent: &Mutex<Sent>) -> MutexGuard<'_, Sent> {
    sent.lock().unwrap_or_else(PoisonError::into_inner)
}

/// This machine's private address, where edges reach its gateway: the configured one, or else the local address of a
/// route towards management.
pub(super) struct PrivateAddress {
    configured: Option<IpAddr>,
    management_url: String,
    /// Where management is reached, once looked up.
    pub(super) management: OnceCell<SocketAddr>,
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
