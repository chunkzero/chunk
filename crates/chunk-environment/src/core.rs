mod archives;
mod runner;
mod sync;

pub(crate) use archives::Archives;
pub use archives::ReleaseArchive;
pub use runner::{CommandLauncher, LaunchSpec, Launcher, READINESS, RELEASE_TIMEOUT, RunnerConfig};

use crate::{PlatformTarget, Running};
use chunk_contract::ControlConnection;
use chunk_proxy::GatewayCredential;
use std::{
    collections::BTreeSet,
    fs, io,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{net::TcpListener, sync::oneshot};
use tokio_util::sync::CancellationToken;

pub struct CoreConfig {
    /// The backend deployment served first. Without one, the backend serves only the deployments it retained.
    pub bundle: Option<PathBuf>,
    pub environment: String,
    /// Holds the backend's store under `backend/`, control's credential and JVM files under `control/`, and the
    /// in-process gateway's ID.
    pub state: PathBuf,
    pub control_record: PathBuf,
    /// The core port, serving control and the sync protocol's `Core` service.
    pub control_bind: SocketAddr,
    /// Where other machines reach the sync protocol's `Core` service, from loopback or private peers only. Unset binds
    /// nothing.
    pub core_bind: Option<SocketAddr>,
    /// This machine's address on the environment's private network, where local JVMs serve players. It must be private.
    pub private_address: Option<IpAddr>,
    /// The token management issued the environment, whose SHA-256 keys machine and operator credentials. Without one,
    /// control's credential keys them.
    pub environment_token: Option<String>,
    /// Drops every control row and control's local files before serving, as when a local session starts over. The
    /// machines of every launch the previous control recorded for remote runners are stopped first, then local JVMs
    /// that outlived it. Only a core with a launcher can stop remote machines, so one without refuses to start while
    /// any are recorded, and so does a core whose stops aren't confirmed.
    pub fresh: bool,
}

/// The environment's backend and the control that writes through its store.
#[derive(Default)]
pub struct Core {
    backend: Option<Running>,
    handle: Option<chunk_backend::Backend>,
    /// The deployment the backend served first, or empty without one.
    deployment: Option<String>,
    control: Option<Running>,
    host: Option<Arc<chunk_control::ProcessHost>>,
    /// Runs control's hosts in place of local JVMs.
    injected_host: Option<Arc<dyn chunk_control::Host>>,
    /// Runs control's hosts on machines a launcher starts, in place of local JVMs.
    runner: Option<Arc<runner::RunnerHost>>,
    authority: Option<chunk_control::server::Ready>,
    /// Every gateway's credential, which the sync protocol authenticates.
    gateways: Arc<sync::Gateways>,
    /// The credential of the gateway serving in this process.
    gateway: Option<GatewayCredential>,
    /// Derives gateway machines' credentials.
    issuer: Option<sync::Issuer>,
    /// The archives of the releases loaded from management.
    archives: Arc<Archives>,
}

impl Core {
    /// Mints the in-process gateway's credential for the gateway ID recorded in the state directory, starts the
    /// backend, calls `on_backend` once it serves, then starts control. On error, everything started is stopped.
    /// # Errors
    /// Reports an unreadable gateway ID, and backend and control startup errors.
    pub async fn start(config: CoreConfig, on_backend: impl FnOnce()) -> io::Result<Self> {
        Self::default().launch(config, on_backend).await
    }

    /// Starts core as [`Self::start`] does, with control running every host on `host` rather than as a local JVM.
    /// For benchmarks, whose synthetic hosts run in process.
    /// # Errors
    /// As [`Self::start`].
    #[doc(hidden)]
    pub async fn start_with_host(config: CoreConfig, host: Arc<dyn chunk_control::Host>) -> io::Result<Self> {
        Self { injected_host: Some(host), ..Self::default() }.launch(config, || {}).await
    }

    /// Starts core as [`Self::start`] does, with control running every host on a machine `runner`'s launcher starts,
    /// whose runner downloads its release from core.
    /// # Errors
    /// As [`Self::start`].
    pub async fn start_with_launcher(config: CoreConfig, runner: RunnerConfig) -> io::Result<Self> {
        let runner = Arc::new(runner::RunnerHost::new(&config.environment, runner));
        Self { runner: Some(runner), ..Self::default() }.launch(config, || {}).await
    }

    async fn launch(self, config: CoreConfig, on_backend: impl FnOnce()) -> io::Result<Self> {
        if let Some(address) = config.private_address.filter(|address| !chunk_service::net::private(*address)) {
            return Err(io::Error::other(format!("{address} is not a private address")));
        }
        let mut core = self;
        let id = gateway_id(&config.state)?;
        core.gateway = Some(GatewayCredential { credential: core.gateways.mint(&id), id });
        let mut started = core.start_backend(&config).await;
        if started.is_ok() {
            on_backend();
            started = core.start_control(&config).await;
        }
        if let Err(error) = started {
            if let Err(error) = core.stop(|| {}).await {
                tracing::error!(%error, "service shutdown failed");
            }
            return Err(error);
        }
        Ok(core)
    }

    async fn start_backend(&mut self, config: &CoreConfig) -> io::Result<()> {
        let stop = CancellationToken::new();
        let (ready, started) = oneshot::channel();
        let backend = chunk_backend::server::Config {
            bundle: config.bundle.clone(),
            environment: config.environment.clone(),
            state: config.state.join("backend"),
        };
        self.backend =
            Some(Running { task: tokio::spawn(chunk_backend::server::run(backend, ready, stop.clone())), stop });
        let ready = Running::ready(&mut self.backend, started, "backend").await?;
        self.handle = Some(ready.backend);
        self.deployment = Some(ready.deployment);
        Ok(())
    }

    /// Starts control. When `fresh`, the previous control's files and discovery record are deleted once every remote
    /// machine and local JVM it launched has confirmed its exit.
    async fn start_control(&mut self, config: &CoreConfig) -> io::Result<()> {
        if config.fresh {
            // Only control's launch records name the remote machines an earlier core started, and stopping local
            // survivors drops them, so remote machines stop first.
            self.stop_remote_machines().await?;
            self.stop_survivors(config).await?;
            // The record names the previous control's credential, which is deleted with its files.
            if_present(fs::remove_file(&config.control_record))?;
            if_present(fs::remove_dir_all(config.state.join("control")))?;
        }
        let listener = TcpListener::bind(config.control_bind).await?;
        let network = match config.core_bind {
            Some(bind) => Some(chunk_control::server::network_listener(bind).await?),
            None => None,
        };
        let host: Arc<dyn chunk_control::Host> = if let Some(host) = &self.injected_host {
            host.clone()
        } else if let Some(runner) = &self.runner {
            runner.clone()
        } else {
            let host = Arc::new(chunk_control::ProcessHost::new(host_config(config)));
            self.host = Some(host.clone());
            host
        };
        self.serve_control(config, listener, network, config.control_record.clone(), host).await?;
        if let (Some(runner), Some(ready), Some(issuer)) = (&self.runner, &self.authority, &self.issuer) {
            runner.attach(&ready.control, issuer.clone(), runner_endpoint(ready, config.private_address));
        }
        Ok(())
    }

    /// Stops the machines of every launch the previous control recorded for remote runners, which only this core's
    /// launcher can do.
    async fn stop_remote_machines(&self) -> io::Result<()> {
        let system = self.system()?;
        let recorded = tokio::task::spawn_blocking(move || chunk_control::recorded_launches(system));
        let hosts = recorded.await.map_err(io::Error::other)?.map_err(io::Error::other)?;
        if hosts.is_empty() {
            return Ok(());
        }
        let Some(runner) = &self.runner else {
            return Err(io::Error::other(format!(
                "not starting fresh: control records {} remote machines; start with the launcher that created them so \
                 core can release them",
                hosts.len()
            )));
        };
        let stopped = runner.stop_recorded(hosts).await;
        stopped.map_err(|error| io::Error::other(format!("not starting fresh: {error}")))
    }

    /// Stops the JVMs of the previous control that may still hold their launch locks, since only a control on its
    /// files can confirm they exited. Each re-attaches only at the endpoint its launch record names, or at the previous
    /// discovery record's endpoint when the record names none, so control serves there, waiting while another process
    /// holds that address. That record stays as it is until they have exited, as control publishes its own elsewhere.
    async fn stop_survivors(&mut self, config: &CoreConfig) -> io::Result<()> {
        let launches = Arc::new(chunk_control::ProcessHost::new(host_config(config)));
        let previous =
            chunk_service::read::<ControlConnection>(&config.control_record).ok().map(|record| record.endpoint);
        let mut waiting = false;
        loop {
            let endpoints = launches.unowned_endpoints().map_err(io::Error::other)?;
            if endpoints.is_empty() {
                return Ok(());
            }
            let known = survivor_bind(&endpoints, previous.as_deref())?;
            let bind = known.unwrap_or(config.control_bind);
            match TcpListener::bind(bind).await {
                Ok(listener) => {
                    if known.is_some() {
                        tracing::warn!(%bind, "stopping JVMs that outlived the previous control");
                    } else {
                        tracing::warn!(%bind, "stopping JVMs that outlived the previous control at an address they may not know");
                    }
                    let record = config.state.join("control").join("recovery.json");
                    // Local JVMs re-attach only to a process host, whatever host this core serves with.
                    self.host = Some(launches.clone());
                    self.serve_control(config, listener, None, record, launches.clone()).await?;
                    return self.stop_control(|| {}).await;
                }
                Err(error) => {
                    if !std::mem::replace(&mut waiting, true) {
                        tracing::warn!(%error, %bind, "JVMs that outlived the previous control re-attach only here; waiting for the address");
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    async fn serve_control(
        &mut self,
        config: &CoreConfig,
        listener: TcpListener,
        network: Option<TcpListener>,
        record: PathBuf,
        host: Arc<dyn chunk_control::Host>,
    ) -> io::Result<()> {
        let backend = self.handle.clone().ok_or_else(|| io::Error::other("backend is not running"))?;
        let stop = CancellationToken::new();
        let (ready, started) = oneshot::channel();
        let control = chunk_control::server::Config {
            connection: record,
            state: config.state.join("control"),
            system: self.system()?,
            listener,
            network,
            control: chunk_control::Config { environment: config.environment.clone() },
            host,
            fresh: config.fresh,
            services: Some(sync::services(
                backend,
                self.gateways.clone(),
                self.archives.clone(),
                config.environment.clone(),
                config.environment_token.clone(),
                config.private_address,
            )),
        };
        self.control =
            Some(Running { task: tokio::spawn(chunk_control::server::run(control, ready, stop.clone())), stop });
        let ready = Running::ready(&mut self.control, started, "control").await?;
        let token = config.environment_token.as_deref();
        self.issuer = Some(sync::Issuer::new(&config.environment, token, &ready.connection.token));
        self.authority = Some(ready);
        Ok(())
    }

    #[must_use]
    pub fn backend(&self) -> Option<chunk_backend::Backend> {
        self.handle.clone()
    }

    /// The environment store's system lane, which control writes through.
    fn system(&self) -> io::Result<chunk_backend::System> {
        self.handle
            .as_ref()
            .map(chunk_backend::Backend::system)
            .ok_or_else(|| io::Error::other("backend is not running"))
    }

    /// The log epoch the backend serves.
    /// # Errors
    /// Reports a stopped backend.
    pub fn epoch(&self) -> io::Result<u64> {
        Ok(self.system()?.epoch().0)
    }

    fn authority(&self) -> io::Result<&chunk_control::server::Ready> {
        self.authority.as_ref().ok_or_else(|| io::Error::other("control is not running"))
    }

    /// # Errors
    /// Reports a stopped control.
    pub fn control_connection(&self) -> io::Result<&ControlConnection> {
        Ok(&self.authority()?.connection)
    }

    /// # Errors
    /// Reports a stopped control.
    pub fn control(&self) -> io::Result<Arc<chunk_control::Control>> {
        Ok(self.authority()?.control.clone())
    }

    /// The archive release `id` was loaded from, kept only for releases downloaded from management. Readers must check
    /// its bytes against its digest as they read.
    #[must_use]
    pub fn release_archive(&self, id: &str) -> Option<ReleaseArchive> {
        self.archives.get(id)
    }

    pub(crate) fn archives(&self) -> &Arc<Archives> {
        &self.archives
    }

    /// Where the network listener serves, if `core_bind` configured one.
    #[must_use]
    pub fn network_address(&self) -> Option<SocketAddr> {
        self.authority.as_ref().and_then(|authority| authority.network)
    }

    /// Gateway `id`'s machine credential, which authenticates it as that gateway until [`Self::revoke_gateway`].
    /// Minting it again returns the same credential.
    /// # Errors
    /// Reports a stopped control or store, and rejects an invalid or revoked ID.
    pub fn gateway_credential(&self, id: &str) -> io::Result<String> {
        let issuer = self.issuer.as_ref().ok_or_else(|| io::Error::other("control is not running"))?;
        self.control()?.add_machine(id, chunk_control::MachineKind::Gateway).map_err(io::Error::other)?;
        Ok(issuer.machine(chunk_control::MachineKind::Gateway, id))
    }

    /// Revokes gateway `id`'s machine credential for good, which ends the streams it opened with `STOPPED`.
    /// # Errors
    /// Reports a stopped control or store, and rejects an unknown gateway.
    pub fn revoke_gateway(&self, id: &str) -> io::Result<()> {
        self.control()?.revoke_machine(id, chunk_control::MachineKind::Gateway).map_err(io::Error::other)
    }

    /// Core's endpoint and the in-process gateway's credential, with the backend deployment it routes players
    /// through.
    /// # Errors
    /// Reports a stopped backend or control.
    pub fn target(&self) -> io::Result<PlatformTarget> {
        let gateway = self.gateway.clone().ok_or_else(|| io::Error::other("core has no gateway"))?;
        Ok(PlatformTarget {
            core: self.control_connection()?.endpoint.clone(),
            gateway,
            deployment: self.deployment.clone().ok_or_else(|| io::Error::other("backend is not running"))?,
        })
    }

    /// Makes `bundle` resident beside earlier versions, retrying while the backend is busy.
    /// # Errors
    /// Reports a stopped, busy or rejecting backend.
    pub async fn deploy(&self, bundle: chunk_contract::Deployment) -> io::Result<()> {
        let backend = self.handle.as_ref().ok_or_else(|| io::Error::other("backend is not running"))?;
        for _ in 0..50 {
            match backend.deploy(bundle.clone()).await {
                Err(chunk_backend::Error::Busy) => tokio::time::sleep(Duration::from_millis(200)).await,
                result => return result.map_err(io::Error::other),
            }
        }
        Err(io::Error::other("backend stayed busy for 10s; deployment not activated"))
    }

    /// Makes `release` control's current release, whose JVMs launch from `distribution`. Earlier releases keep their
    /// sessions.
    /// # Errors
    /// Reports a stopped control or a rejected release.
    pub fn activate(
        &self,
        deployment: &str,
        distribution: chunk_control::Distribution,
        release: chunk_control::Release,
    ) -> io::Result<()> {
        if let Some(host) = &self.host {
            host.add_release(deployment, distribution).map_err(io::Error::other)?;
        }
        self.control()?.activate_release(release).map_err(io::Error::other)
    }

    /// Whether the backend stopped.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.backend.as_ref().is_some_and(|backend| backend.task.is_finished())
    }

    #[must_use]
    pub fn control_failed(&self) -> bool {
        self.control.as_ref().is_some_and(|control| control.task.is_finished())
    }

    /// Stops every JVM, then control, retrying until each JVM has confirmed its exit, however long that takes. The
    /// host and backend stay until then. `on_wait` runs once, when a first attempt fails.
    /// # Errors
    /// Reports control shutdown errors.
    pub async fn stop_control(&mut self, on_wait: impl Fn()) -> io::Result<()> {
        let mut waiting = false;
        // Kept until remote machines have stopped, since only control's launch records name some of them.
        let control = self.control().ok();
        if let Some(control) = &control {
            // While control serves, a JVM that outlived an earlier control can still re-attach and be stopped.
            while let Err(error) = control.shutdown().await
                && !self.control_failed()
            {
                wait_for_jvms(&on_wait, &mut waiting, &error).await;
            }
        }
        self.authority = None;
        let mut result = Ok(());
        if let Some(control) = self.control.take()
            && let Err(error) = control.stop().await
        {
            tracing::error!(%error, "control shutdown failed");
            result = Err(error);
        }
        if let Some(host) = &self.host {
            while let Err(error) = host.shutdown().await {
                wait_for_jvms(&on_wait, &mut waiting, &error).await;
            }
        }
        self.host = None;
        // Stops the remote machines control could not, as once its store stopped.
        if let Some(runner) = &self.runner {
            while let Err(error) = runner.shutdown().await {
                wait_for_jvms(&on_wait, &mut waiting, &error).await;
            }
        }
        drop(control);
        result
    }

    /// Stops control as [`Self::stop_control`] does, then the backend.
    /// # Errors
    /// Reports the last control or backend shutdown error.
    pub async fn stop(mut self, on_wait: impl Fn()) -> io::Result<()> {
        let mut result = self.stop_control(on_wait).await;
        // The service joins the backend engine only once this last outside handle is gone.
        if let Some(handle) = self.handle.take() {
            tokio::task::spawn_blocking(move || drop(handle)).await.map_err(io::Error::other)?;
        }
        if let Some(backend) = self.backend.take()
            && let Err(error) = backend.stop().await
        {
            result = Err(error);
        }
        result
    }
}

/// Where control recovers JVMs launched with `endpoints`: the endpoint they share, taking `previous` for launch records
/// that name none, or `None` when no endpoint is known. A control launches JVMs only after every earlier one has
/// exited, so they share at most one.
fn survivor_bind(endpoints: &BTreeSet<Option<String>>, previous: Option<&str>) -> io::Result<Option<SocketAddr>> {
    let known: BTreeSet<_> = endpoints.iter().filter_map(|endpoint| endpoint.as_deref().or(previous)).collect();
    let mut known = known.into_iter();
    match (known.next(), known.next()) {
        (None, _) => Ok(None),
        (Some(endpoint), None) => address(endpoint)
            .map(Some)
            .ok_or_else(|| io::Error::other(format!("surviving JVM has invalid control endpoint {endpoint}"))),
        (Some(_), Some(_)) => Err(io::Error::other("surviving JVMs were given different control endpoints")),
    }
}

/// Where runners on other machines reach core: its network listener, at the private address when it binds an
/// unspecified one, and otherwise control's endpoint.
fn runner_endpoint(ready: &chunk_control::server::Ready, private: Option<IpAddr>) -> String {
    match (ready.network, private) {
        (Some(network), _) if !network.ip().is_unspecified() => format!("http://{network}"),
        (Some(network), Some(private)) => format!("http://{}", SocketAddr::new(private, network.port())),
        _ => ready.connection.endpoint.clone(),
    }
}

fn address(endpoint: &str) -> Option<SocketAddr> {
    endpoint.strip_prefix("http://")?.parse().ok()
}

fn host_config(config: &CoreConfig) -> chunk_control::ProcessHostConfig {
    chunk_control::ProcessHostConfig {
        directory: config.state.join("control").join("nodes"),
        environment: config.environment.clone(),
        private_address: config.private_address,
    }
}

/// The in-process gateway's ID, recorded in `state` on first start so that the claims it holds outlive a restart.
fn gateway_id(state: &Path) -> io::Result<String> {
    let path = state.join("gateway-id");
    match fs::read_to_string(&path) {
        Ok(id) => Ok(id),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let id = uuid::Uuid::new_v4().to_string();
            fs::create_dir_all(state)?;
            let written = path.with_extension("tmp");
            fs::write(&written, &id)?;
            fs::rename(written, path)?;
            Ok(id)
        }
        Err(error) => Err(error),
    }
}

fn if_present(removed: io::Result<()>) -> io::Result<()> {
    match removed {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Waits before the next attempt to stop the JVMs, calling `on_wait` the first time.
async fn wait_for_jvms(on_wait: &impl Fn(), waiting: &mut bool, error: &chunk_control::Error) {
    if !std::mem::replace(waiting, true) {
        tracing::warn!(%error, "JVM exit unconfirmed; retrying until every JVM stops");
        on_wait();
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
}
