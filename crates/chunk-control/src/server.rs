//! Embeddable control server with explicit host ownership.
use crate::{Control, ControlConnection, Host, Operations};
use std::{io, path::PathBuf, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::oneshot};
use tokio_stream::{StreamExt, wrappers::TcpListenerStream};
use tokio_util::sync::CancellationToken;

/// Builds the services control serves on its listener, from control, its credential, a token cancelled when the
/// transport begins shutting down, after hosts have stopped, which must end their open streams, and control's accepted
/// operations, which it awaits before it stops hosts.
pub type Services =
    Box<dyn FnOnce(&Arc<Control>, &str, CancellationToken, Operations) -> tonic::service::Routes + Send>;

pub struct Config {
    /// Holds the credential and the host's local files; durable state lives in the environment's store.
    pub state: PathBuf,
    pub system: chunk_backend::System,
    pub connection: PathBuf,
    /// Loopback listener control serves on.
    pub listener: TcpListener,
    pub control: crate::Config,
    pub host: Arc<dyn Host>,
    /// Drops every control row before serving, as when a local session starts over.
    pub fresh: bool,
    pub services: Option<Services>,
}

/// A serving control, which activates and retires releases.
pub struct Ready {
    pub connection: ControlConnection,
    pub control: Arc<Control>,
}

/// Serves control requests and awaits accepted operations before stopping hosts. Stops once the environment store
/// can no longer commit, such as after another store took over the environment.
/// # Errors
/// Reports configuration, durable-state, transport and shutdown errors, and a stopped environment store.
pub async fn run(config: Config, ready: oneshot::Sender<Ready>, stop: CancellationToken) -> io::Result<()> {
    let listener = config.listener;
    let address = listener.local_addr()?;
    if !address.ip().is_loopback() {
        return Err(io::Error::other("control must bind loopback"));
    }
    config.host.configure(format!("http://{address}")).map_err(io::Error::other)?;
    let path = config.connection;
    let services = config.services;
    let (control, token) = tokio::task::spawn_blocking(move || -> io::Result<_> {
        std::fs::create_dir_all(&config.state)?;
        let token = chunk_service::secret(&config.state.join("token"))?;
        let control =
            Control::open(config.system, config.control, config.host, config.fresh).map_err(io::Error::other)?;
        Ok((control, token))
    })
    .await
    .map_err(io::Error::other)??;
    let operations = Operations::default();
    let executor = CancellationToken::new();
    let capacity = tokio::spawn({
        let (control, executor) = (control.clone(), executor.clone());
        async move { control.run_capacity(&executor).await }
    });
    // Ends the transport and the services' open streams once hosts have stopped, so a JVM following its topic is still
    // sent `stop`.
    let transport = CancellationToken::new();
    // Cancelled once control stops reconciling, so hosts can stop.
    let closing = CancellationToken::new();
    let serve = Box::pin(async {
        let connection = ControlConnection { endpoint: format!("http://{address}"), token };
        if path.exists() {
            let old: ControlConnection = chunk_service::read(&path)?;
            if old.token != connection.token {
                return Err(io::Error::other("connection file belongs to another control authority"));
            }
        }
        let _record = chunk_service::Record::publish(&path, &connection)?;
        let routes = services.map_or_else(tonic::service::Routes::default, |services| {
            services(&control, &connection.token, transport.clone(), operations.clone())
        });
        let _ = ready.send(Ready { connection, control: control.clone() });
        let connections = chunk_service::Connections::default();
        let incoming = TcpListenerStream::new(listener).map(|stream| {
            let stream = stream?;
            stream.set_nodelay(true)?;
            Ok::<_, io::Error>(connections.track(stream))
        });
        let server = tonic::transport::Server::builder()
            .add_routes(routes)
            .serve_with_incoming_shutdown(incoming, transport.clone().cancelled_owned());
        tokio::pin!(server);
        let reconcile = reconcile(&control, &stop);
        tokio::pin!(reconcile);
        let health = monitor_health(&control, &stop);
        tokio::pin!(health);
        tracing::info!(%address, "control ready");
        let result = tokio::select! {
            result = &mut server => {
                stop.cancel();
                return result.map_err(io::Error::other).and(reconcile.await);
            }
            () = &mut health => reconcile.await,
            reconciled = &mut reconcile => reconciled,
        };
        stop.cancel();
        closing.cancel();
        tokio::select! {
            exited = &mut server => return result.and(exited.map_err(io::Error::other)),
            () = transport.cancelled() => {}
        }
        result.and(connections.drain("control", server).await.map_err(io::Error::other))
    });
    let serving = async {
        let result = serve.await;
        closing.cancel();
        result
    };
    let stopping = stop_hosts(&control, &operations, capacity, &executor, &closing, &transport);
    let (result, stopped) = tokio::join!(serving, stopping);
    result.and(stopped).and(control.close().map_err(io::Error::other))
}

/// Once `closing` is cancelled, stops admitting operations, awaits accepted ones, stops the capacity executor and
/// then every host, and only then cancels `transport`.
async fn stop_hosts(
    control: &Control,
    operations: &Operations,
    capacity: tokio::task::JoinHandle<()>,
    executor: &CancellationToken,
    closing: &CancellationToken,
    transport: &CancellationToken,
) -> io::Result<()> {
    closing.cancelled().await;
    // Otherwise new operations could keep the tracker from ever emptying.
    control.stop_admitting();
    operations.close();
    operations.wait().await;
    // Accepted operations may wait on capacity, so the executor stops after them and before hosts stop.
    executor.cancel();
    let executed = capacity.await.map_err(io::Error::other);
    let stopped = control.shutdown().await.map_err(io::Error::other);
    transport.cancel();
    executed.and(stopped)
}

/// Reconciles control every 2 seconds until `stop` is cancelled, or fails once the environment store has stopped.
async fn reconcile(control: &Arc<Control>, stop: &CancellationToken) -> io::Result<()> {
    let mut timer = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! { () = stop.cancelled() => return Ok(()), _ = timer.tick() => {} }
        if control.store_stopped() {
            tracing::error!("environment store stopped; stopping control");
            return Err(io::Error::other("environment store stopped"));
        }
        if let Err(error) = control.reconcile_all().await {
            tracing::warn!(%error, "control reconciliation unavailable");
        }
    }
}

pub(super) async fn monitor_health(control: &Arc<Control>, stop: &CancellationToken) {
    let mut timer = tokio::time::interval(Duration::from_secs(5));
    // Missed samples must not turn one JVM tick into several failed health checks.
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! { () = stop.cancelled() => break, _ = timer.tick() => {} }
        if let Err(error) = control.poll_health() {
            tracing::warn!(%error, "node health poll failed");
        }
    }
}
