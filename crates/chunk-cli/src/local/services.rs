use super::{
    Reporter, Settings,
    report::{self, Deployment},
};
use chunk_build::Release;
use chunk_proto::v1::{NodeStatus, NodesRequest};
use std::{io, sync::Arc, time::Duration};
use tokio::{sync::oneshot, task::JoinHandle};
use tokio_util::sync::CancellationToken;

type Task = JoinHandle<io::Result<()>>;

const STARTUP: Duration = Duration::from_secs(30);

struct Service {
    stop: CancellationToken,
    task: Task,
}
impl Service {
    async fn ready<T>(slot: &mut Option<Self>, started: oneshot::Receiver<T>, name: &str) -> io::Result<T> {
        match tokio::time::timeout(STARTUP, started).await {
            Ok(Ok(connection)) => return Ok(connection),
            Ok(Err(_)) => {}
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{name} did not become ready within {}s", STARTUP.as_secs()),
                ));
            }
        }
        let result = (&mut slot.as_mut().expect("service started").task).await;
        slot.take();
        match result {
            Ok(Err(error)) => Err(error),
            Err(error) => Err(io::Error::other(error)),
            Ok(Ok(())) => Err(io::Error::other(format!("{name} stopped before readiness"))),
        }
    }

    async fn stop(self) -> io::Result<()> {
        self.stop.cancel();
        self.task.await.map_err(io::Error::other)?
    }
}

#[derive(Default)]
struct Services {
    backend: Option<Service>,
    control: Option<Service>,
    edge: Option<Service>,
    host: Option<Arc<chunk_control::ProcessHost>>,
    control_connection: Option<chunk_contract::ControlConnection>,
}
impl Services {
    async fn start(
        &mut self,
        options: &Settings,
        authority: &chunk_control::Config,
        artifact: &Release,
        reporter: &Reporter,
    ) -> io::Result<()> {
        let token = CancellationToken::new();
        let (ready, started) = oneshot::channel();
        let config = chunk_backend::server::Config {
            bundle: artifact.directory.join("backend.json"),
            environment: authority.deployment.environment.clone(),
            state: options.state.join("backend"),
            connection: options.state.join("backend.json"),
            bind: options.backend_bind,
        };
        self.backend =
            Some(Service { task: tokio::spawn(chunk_backend::server::run(config, ready, token.clone())), stop: token });
        let backend_connection = Service::ready(&mut self.backend, started, "backend").await?;
        reporter.done("Backend", &backend_connection.endpoint);
        let control_state = options.state.join("control").join(&artifact.id);
        let embedded = Arc::new(chunk_control::ProcessHost::new(chunk_control::ProcessHostConfig {
            distribution: artifact.directory.clone(),
            java: options.java.clone(),
            directory: control_state.join("nodes"),
            deployment: authority.deployment.clone(),
            apps: authority.apps.clone(),
            profiles: authority.profiles.clone(),
            backend: backend_connection.clone(),
        }));
        self.host = Some(embedded.clone());
        let token = CancellationToken::new();
        let (ready, started) = oneshot::channel();
        let config = chunk_control::server::Config {
            state: control_state,
            connection: options.state.join("control.json"),
            bind: options.control_bind,
            control: authority.clone(),
            host: embedded,
        };
        self.control =
            Some(Service { task: tokio::spawn(chunk_control::server::run(config, ready, token.clone())), stop: token });
        let control_connection = Service::ready(&mut self.control, started, "control").await?;
        reporter.done("Control", &control_connection.endpoint);
        self.control_connection = Some(control_connection.clone());
        let proxy = chunk_edge::Proxy::bind(
            options.bind,
            chunk_edge::ProxyConfig {
                platform: Some(chunk_edge::PlatformTarget { backend: backend_connection, control: control_connection }),
                ..Default::default()
            },
        )
        .await?;
        reporter.done("Proxy", options.bind);
        let token = CancellationToken::new();
        let shutdown = token.clone();
        self.edge = Some(Service {
            task: tokio::spawn(proxy.run(async move {
                shutdown.cancelled().await;
                Ok(())
            })),
            stop: token,
        });
        Ok::<_, io::Error>(())
    }
    fn failed(&self) -> bool {
        [&self.backend, &self.control, &self.edge].into_iter().flatten().any(|service| service.task.is_finished())
    }
    async fn stop(self) -> io::Result<()> {
        let mut result = Ok(());
        for service in [self.edge, self.control].into_iter().flatten() {
            if let Err(error) = service.stop().await {
                tracing::error!(%error, "service shutdown failed");
                result = Err(error);
            }
        }
        if let Some(host) = self.host
            && let Err(error) = host.shutdown().await
        {
            result = Err(io::Error::other(error));
        }
        if let Some(backend) = self.backend
            && let Err(error) = backend.stop().await
        {
            result = Err(error);
        }
        result
    }
}

pub(super) async fn run(
    options: &Settings,
    control: &chunk_control::Config,
    artifact: &Release,
    reporter: &Reporter,
    stop: CancellationToken,
) -> io::Result<()> {
    let mut services = Services::default();
    let started = tokio::select! {
        result = services.start(options, control, artifact, reporter) => result,
        () = stop.cancelled() => Ok(()),
    };
    let result = if started.is_ok() && !stop.is_cancelled() {
        reporter.done(
            "Ready",
            format!(
                "connect to {} · JVM logs in {} · Ctrl-C stops",
                options.bind,
                options.state.join("control").join(&artifact.id).join("nodes").display()
            ),
        );
        let mut poll = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                () = stop.cancelled() => break Ok(()),
                _ = poll.tick() => {
                    if let Some(connection) = &services.control_connection {
                        let nodes = nodes(connection).await.unwrap_or_default();
                        reporter.deployments(vec![Deployment { id: artifact.id.clone(), state: "current".into(), nodes }]);
                    }
                }
                () = tokio::time::sleep(Duration::from_millis(100)) => {
                    if services.failed() { break Err(io::Error::other("local service stopped")); }
                }
            }
        }
    } else {
        started
    };
    reporter.running("Stop");
    let started = std::time::Instant::now();
    let stopped = services.stop().await;
    reporter.done("Stop", report::seconds(started.elapsed()));
    result.and(stopped)
}

/// The nodes a control authority reports, or none while it is unreachable.
async fn nodes(connection: &chunk_contract::ControlConnection) -> io::Result<Vec<NodeStatus>> {
    let request = async {
        let mut client = crate::players::client(connection).await?;
        let request = crate::players::auth(NodesRequest {}, &connection.token)?;
        Ok(client.nodes(request).await.map_err(io::Error::other)?.into_inner().nodes)
    };
    tokio::time::timeout(Duration::from_secs(2), request).await.map_err(io::Error::other)?
}

#[cfg(test)]
mod tests;
