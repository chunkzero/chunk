//! Embeddable control server with explicit host ownership.
use crate::{Control, ControlConnection, Host, Service};
use chunk_proto::v1::local_control_server::LocalControlServer;
use std::{io, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::oneshot};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;

pub struct Config {
    pub state: PathBuf,
    pub connection: PathBuf,
    pub bind: SocketAddr,
    pub control: crate::Config,
    pub host: Arc<dyn Host>,
}

/// Serves control requests and awaits accepted operations before stopping hosts.
/// # Errors
/// Reports configuration, durable-state, transport and shutdown errors.
pub async fn run(config: Config, ready: oneshot::Sender<ControlConnection>, stop: CancellationToken) -> io::Result<()> {
    if !config.bind.ip().is_loopback() {
        return Err(io::Error::other("control must bind loopback"));
    }
    let listener = TcpListener::bind(config.bind).await?;
    let address = listener.local_addr()?;
    config.host.configure(format!("http://{address}")).map_err(io::Error::other)?;
    let path = config.connection;
    let (control, token) = tokio::task::spawn_blocking(move || -> io::Result<_> {
        std::fs::create_dir_all(&config.state)?;
        let token = chunk_service::secret(&config.state.join("token"))?;
        let control = Control::open(&config.state.join("directory.sqlite"), config.control, config.host)
            .map_err(io::Error::other)?;
        Ok((control, token))
    })
    .await
    .map_err(io::Error::other)??;
    let service = Service::new(control.clone(), token.clone()).map_err(io::Error::other)?;
    let operations = service.operations();
    let result = async {
        let connection = ControlConnection { endpoint: format!("http://{address}"), token };
        if path.exists() {
            let old: ControlConnection = chunk_service::read(&path)?;
            if old.token != connection.token {
                return Err(io::Error::other("connection file belongs to another control authority"));
            }
        }
        let _record = chunk_service::Record::publish(&path, &connection)?;
        let _ = ready.send(connection);
        let server = tonic::transport::Server::builder()
            .add_service(
                LocalControlServer::new(service.clone())
                    .max_decoding_message_size(65_536)
                    .max_encoding_message_size(8 * 1024 * 1024),
            )
            .add_service(
                chunk_proto::v1::supervisor_server::SupervisorServer::new(service.clone())
                    .max_decoding_message_size(65_536),
            )
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), stop.clone().cancelled_owned());
        tokio::pin!(server);
        let reconcile = async {
            let mut timer = tokio::time::interval(Duration::from_secs(2));
            loop {
                tokio::select! { () = stop.cancelled() => break, _ = timer.tick() => {} }
                if let Err(error) = control.reconcile_all().await {
                    tracing::warn!(%error, "control reconciliation unavailable");
                }
            }
        };
        tokio::pin!(reconcile);
        let health = monitor_health(&control, &stop);
        tokio::pin!(health);
        tracing::info!(%address, "control ready");
        let result = tokio::select! {
            () = &mut health => Ok(()),
            result = &mut server => result.map_err(io::Error::other),
            () = &mut reconcile => {
                return match tokio::time::timeout(Duration::from_secs(5), &mut server).await {
                    Ok(result) => result.map_err(io::Error::other),
                    Err(_) => Err(io::Error::other("control transport shutdown timed out")),
                };
            }
        };
        stop.cancel();
        reconcile.await;
        result
    }
    .await;
    service.close_methods();
    operations.close();
    operations.wait().await;
    let stopped = control.shutdown().await.map_err(io::Error::other);
    result.and(stopped)
}

pub(super) async fn monitor_health(control: &Arc<Control>, stop: &CancellationToken) {
    let mut timer = tokio::time::interval(Duration::from_secs(5));
    // Missed samples must not turn one JVM tick into several failed health checks.
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! { () = stop.cancelled() => break, _ = timer.tick() => {} }
        if let Err(error) = control.poll_health().await {
            tracing::warn!(%error, "node health poll failed");
        }
    }
}
