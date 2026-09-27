mod sync;

use crate::{PlatformTarget, Running};
use chunk_contract::{BackendConnection, ControlConnection};
use chunk_proxy::GatewayCredential;
use std::{
    collections::BTreeSet,
    fs, io,
    net::SocketAddr,
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
    pub backend_record: PathBuf,
    pub control_record: PathBuf,
    pub backend_bind: SocketAddr,
    /// The core port, serving control and the sync protocol's `Core` service.
    pub control_bind: SocketAddr,
    /// Drops every control row and control's local files before serving, as when a local session starts over. JVMs
    /// that outlived the previous control are stopped first.
    pub fresh: bool,
}

/// The environment's backend and the control that writes through its store.
#[derive(Default)]
pub struct Core {
    backend: Option<Running>,
    handle: Option<chunk_backend::Backend>,
    connection: Option<BackendConnection>,
    control: Option<Running>,
    host: Option<Arc<chunk_control::ProcessHost>>,
    /// Runs control's hosts in place of local JVMs.
    injected_host: Option<Arc<dyn chunk_control::Host>>,
    authority: Option<chunk_control::server::Ready>,
    /// Every gateway's credential, which the sync protocol authenticates.
    gateways: Arc<sync::Gateways>,
    /// The credential of the gateway serving in this process.
    gateway: Option<GatewayCredential>,
}

impl Core {
    /// Mints the in-process gateway's credential for the gateway ID recorded in the state directory, starts the
    /// backend, calls `on_backend` once it serves, then starts control. On error, everything started is stopped.
    /// # Errors
    /// Reports an unreadable gateway ID, and backend and control startup errors.
    pub async fn start(config: CoreConfig, on_backend: impl FnOnce(&BackendConnection)) -> io::Result<Self> {
        Self::default().launch(config, on_backend).await
    }

    /// Starts core as [`Self::start`] does, with control running every host on `host` rather than as a local JVM.
    /// For benchmarks, whose synthetic hosts run in process.
    /// # Errors
    /// As [`Self::start`].
    #[doc(hidden)]
    pub async fn start_with_host(config: CoreConfig, host: Arc<dyn chunk_control::Host>) -> io::Result<Self> {
        Self { injected_host: Some(host), ..Self::default() }.launch(config, |_| {}).await
    }

    async fn launch(self, config: CoreConfig, on_backend: impl FnOnce(&BackendConnection)) -> io::Result<Self> {
        let mut core = self;
        let id = gateway_id(&config.state)?;
        core.gateway = Some(GatewayCredential { credential: core.gateways.mint(&id), id });
        let mut started = core.start_backend(&config).await;
        if started.is_ok() {
            on_backend(core.backend_connection()?);
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
            connection: config.backend_record.clone(),
            bind: config.backend_bind,
        };
        self.backend =
            Some(Running { task: tokio::spawn(chunk_backend::server::run(backend, ready, stop.clone())), stop });
        let ready = Running::ready(&mut self.backend, started, "backend").await?;
        self.handle = Some(ready.backend);
        self.connection = Some(ready.connection);
        Ok(())
    }

    /// Starts control. When `fresh`, the previous control's files and discovery record are deleted once every JVM it
    /// launched has confirmed its exit.
    async fn start_control(&mut self, config: &CoreConfig) -> io::Result<()> {
        if config.fresh {
            self.stop_survivors(config).await?;
            // The record names the previous control's credential, which is deleted with its files.
            if_present(fs::remove_file(&config.control_record))?;
            if_present(fs::remove_dir_all(config.state.join("control")))?;
        }
        let listener = TcpListener::bind(config.control_bind).await?;
        self.serve_control(config, listener, config.control_record.clone()).await
    }

    /// Stops the JVMs of the previous control that may still hold their launch locks, since only a control on its
    /// files can confirm they exited. Each re-attaches only at the endpoint its launch record names, or at the previous
    /// discovery record's endpoint when the record names none, so control serves there, waiting while another process
    /// holds that address. That record stays as it is until they have exited, as control publishes its own elsewhere.
    async fn stop_survivors(&mut self, config: &CoreConfig) -> io::Result<()> {
        let launches = chunk_control::ProcessHost::new(self.host_config(config)?);
        let previous =
            chunk_service::read::<ControlConnection>(&config.control_record).ok().map(|record| record.endpoint);
        let backend = address(&self.backend_connection()?.endpoint);
        let mut waiting = false;
        loop {
            let endpoints = launches.unowned_endpoints().map_err(io::Error::other)?;
            if endpoints.is_empty() {
                return Ok(());
            }
            let known = survivor_bind(&endpoints, previous.as_deref())?;
            let bind = known.unwrap_or(config.control_bind);
            if backend.is_some_and(|backend| overlaps(backend, bind)) {
                return Err(io::Error::other(format!(
                    "JVMs that outlived the previous control re-attach only at {bind}, where the backend now serves; \
                     choose another backend address"
                )));
            }
            match TcpListener::bind(bind).await {
                Ok(listener) => {
                    if known.is_some() {
                        tracing::warn!(%bind, "stopping JVMs that outlived the previous control");
                    } else {
                        tracing::warn!(%bind, "stopping JVMs that outlived the previous control at an address they may not know");
                    }
                    let record = config.state.join("control").join("recovery.json");
                    self.serve_control(config, listener, record).await?;
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

    fn host_config(&self, config: &CoreConfig) -> io::Result<chunk_control::ProcessHostConfig> {
        let directory = config.state.join("control").join("nodes");
        Ok(chunk_control::ProcessHostConfig { directory, backend: self.backend_connection()?.clone() })
    }

    async fn serve_control(&mut self, config: &CoreConfig, listener: TcpListener, record: PathBuf) -> io::Result<()> {
        let host: Arc<dyn chunk_control::Host> = if let Some(host) = &self.injected_host {
            host.clone()
        } else {
            let host = Arc::new(chunk_control::ProcessHost::new(self.host_config(config)?));
            self.host = Some(host.clone());
            host
        };
        let backend = self.handle.clone().ok_or_else(|| io::Error::other("backend is not running"))?;
        let stop = CancellationToken::new();
        let (ready, started) = oneshot::channel();
        let control = chunk_control::server::Config {
            connection: record,
            state: config.state.join("control"),
            system: self.system()?,
            listener,
            control: chunk_control::Config { environment: config.environment.clone() },
            host,
            fresh: config.fresh,
            services: Some(sync::services(backend, self.gateways.clone())),
        };
        self.control =
            Some(Running { task: tokio::spawn(chunk_control::server::run(control, ready, stop.clone())), stop });
        self.authority = Some(Running::ready(&mut self.control, started, "control").await?);
        Ok(())
    }

    /// # Errors
    /// Reports a stopped backend.
    pub fn backend_connection(&self) -> io::Result<&BackendConnection> {
        self.connection.as_ref().ok_or_else(|| io::Error::other("backend is not running"))
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

    /// Core's endpoint and the in-process gateway's credential, with the backend deployment it routes players
    /// through.
    /// # Errors
    /// Reports a stopped backend or control.
    pub fn target(&self) -> io::Result<PlatformTarget> {
        let gateway = self.gateway.clone().ok_or_else(|| io::Error::other("core has no gateway"))?;
        Ok(PlatformTarget {
            core: self.control_connection()?.endpoint.clone(),
            gateway,
            deployment: self.backend_connection()?.deployment.clone(),
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
        let host = self.host.as_ref().ok_or_else(|| io::Error::other("control is not running"))?;
        host.add_release(deployment, distribution).map_err(io::Error::other)?;
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
        if let Ok(control) = self.control() {
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

fn address(endpoint: &str) -> Option<SocketAddr> {
    endpoint.strip_prefix("http://")?.parse().ok()
}

/// Whether a listener on `served` keeps `bind` from binding.
fn overlaps(served: SocketAddr, bind: SocketAddr) -> bool {
    served.port() == bind.port() && (served.ip() == bind.ip() || served.ip().is_unspecified())
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
