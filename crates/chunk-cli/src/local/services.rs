use super::Settings;
use chunk_build::Release;
use std::{io, sync::Arc};
use tokio::{sync::oneshot, task::JoinHandle};
use tokio_util::sync::CancellationToken;

type Task = JoinHandle<io::Result<()>>;

struct Service {
    stop: CancellationToken,
    task: Task,
}
impl Service {
    async fn ready<T>(slot: &mut Option<Self>, started: oneshot::Receiver<T>, name: &str) -> io::Result<T> {
        if let Ok(connection) = started.await {
            return Ok(connection);
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
    host: Option<Arc<chunk_control::EmbeddedHost>>,
}
impl Services {
    async fn start(
        &mut self,
        options: &Settings,
        authority: &chunk_control::Config,
        artifact: &Release,
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
        let control_state = options.state.join("control").join(&artifact.id);
        let embedded = Arc::new(chunk_control::EmbeddedHost::new(
            chunk_runtime::server::Config {
                distribution: artifact.directory.join("gameplay"),
                java: options.java.clone(),
                connection: control_state.join("runtimes/runtime.json"),
                deployment: authority.deployment.clone(),
                machine_profile: String::new(),
                artifact_digest: artifact.id.clone(),
                memory_mib: 512,
                backend: Some(backend_connection.clone()),
            },
            authority.profiles.clone(),
        ));
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
        let proxy = chunk_edge::Proxy::bind(
            options.bind,
            chunk_edge::ProxyConfig {
                platform: Some(chunk_edge::PlatformTarget { backend: backend_connection, control: control_connection }),
                ..Default::default()
            },
        )
        .await?;
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
    stop: CancellationToken,
) -> io::Result<()> {
    let mut services = Services::default();
    let started = tokio::select! { result = services.start(options, control, artifact) => result, () = stop.cancelled() => Ok(()) };
    let result = if started.is_ok() && !stop.is_cancelled() {
        tracing::info!(address = %options.bind, "local project ready; Ctrl-C stops all services");
        loop {
            tokio::select! {
                () = stop.cancelled() => break Ok(()),
                () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                    if services.failed() { break Err(io::Error::other("local service stopped")); }
                }
            }
        }
    } else {
        started
    };
    let stopped = services.stop().await;
    result.and(stopped)
}

#[cfg(test)]
mod tests;
