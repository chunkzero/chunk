use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use chunk_contract::BackendConnection;
use chunk_control::ControlConnection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::oneshot,
};
use tokio_util::sync::CancellationToken;

use crate::{
    backend,
    config::{Config, Scenario},
    control, proxy, sync,
};

#[derive(Serialize, Deserialize)]
pub struct Init {
    pub config: Config,
    /// Synthetic gameplay/runtime address, or the compiled deployment path for backend workloads.
    pub backend: String,
    pub state: PathBuf,
    pub output: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub struct Ready {
    pub endpoint: String,
    pub control: Option<ControlConnection>,
    pub backend: Option<BackendConnection>,
    pub sync: Option<sync::Connection>,
}

pub struct Target {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    pub ready: Ready,
}

impl Target {
    pub async fn start(config: &Config, backend: String, state: &Path, output: &Path) -> Result<Self> {
        let executable = std::env::current_exe()?;
        let mut command = if let Some(cpus) = &config.target_cpus {
            let mut command = Command::new("taskset");
            command.args(["-c", cpus]).arg(executable);
            command
        } else {
            Command::new(executable)
        };
        let mut child = command
            .arg("--worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdin = child.stdin.take().context("target stdin")?;
        let init = Init { config: config.clone(), backend, state: state.into(), output: output.into() };
        stdin.write_all(format!("{}\n", serde_json::to_string(&init)?).as_bytes()).await?;
        let mut stdout = BufReader::new(child.stdout.take().context("target stdout")?);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(30), stdout.read_line(&mut line)).await??;
        let ready = serde_json::from_str(&line).context("target exited without readiness")?;
        Ok(Self { child, stdin, stdout, ready })
    }

    pub fn pid(&self) -> Result<u32> {
        self.child.id().context("target already exited")
    }

    async fn command(&mut self, command: &str) -> Result<Value> {
        self.stdin.write_all(format!("{command}\n").as_bytes()).await?;
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(30), self.stdout.read_line(&mut line)).await??;
        serde_json::from_str(&line).with_context(|| format!("target did not answer {command}"))
    }

    /// Clears target phase timings, e.g. after warmup.
    pub async fn reset(&mut self) -> Result<()> {
        self.command("reset").await.map(drop)
    }

    /// Target phase timings recorded since the last reset; empty for workloads without them.
    pub async fn report(&mut self) -> Result<Value> {
        self.command("report").await
    }

    pub async fn stop(mut self) -> Result<()> {
        self.stdin.write_all(b"stop\n").await?;
        if let Ok(status) = tokio::time::timeout(Duration::from_secs(15), self.child.wait()).await {
            ensure!(status?.success(), "benchmark target failed");
        } else {
            self.child.kill().await?;
            anyhow::bail!("benchmark target shutdown timed out");
        }
        Ok(())
    }
}

pub async fn serve(init: Init) -> Result<()> {
    init.config.validate()?;
    let stop = CancellationToken::new();
    let mut tasks = tokio::task::JoinSet::new();
    let ready = if init.config.scenario == Scenario::ProxyRelay {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let ready = Ready { endpoint: listener.local_addr()?.to_string(), control: None, backend: None, sync: None };
        let token = stop.clone();
        let (address, config) = (init.backend.clone(), init.config.clone());
        tasks.spawn(async move { proxy::target(listener, &address, &config, token).await });
        ready
    } else if init.config.scenario == Scenario::SyncQueries {
        chunk_backend::observe(backend::observe);
        let state = init.state.join("core");
        let config = chunk_environment::CoreConfig {
            bundle: Some(init.backend.clone().into()),
            environment: backend::ENVIRONMENT.into(),
            backend_record: state.join("backend.json"),
            control_record: state.join("control.json"),
            state,
            backend_bind: "127.0.0.1:0".parse()?,
            control_bind: "127.0.0.1:0".parse()?,
            fresh: false,
        };
        let core = chunk_environment::Core::start(config, |_| {}).await?;
        let control = core.control_connection()?;
        let gateway = core.target()?.gateway.context("in-process gateway credential")?.credential;
        let connection = sync::Connection { endpoint: control.endpoint.clone(), cli: control.token.clone(), gateway };
        let token = stop.clone();
        tasks.spawn(async move {
            token.cancelled().await;
            Ok(core.stop(|| {}).await?)
        });
        Ready { endpoint: connection.endpoint.clone(), control: None, backend: None, sync: Some(connection) }
    } else if init.config.scenario.is_backend() {
        chunk_backend::observe(backend::observe);
        let (ready, receiver) = oneshot::channel();
        let config = chunk_backend::server::Config {
            bundle: Some(init.backend.clone().into()),
            environment: backend::ENVIRONMENT.into(),
            state: init.state.join("backend"),
            connection: init.state.join("backend-connection.json"),
            bind: "127.0.0.1:0".parse()?,
        };
        let token = stop.clone();
        tasks.spawn(async move { Ok(chunk_backend::server::run(config, ready, token).await?) });
        let connection = receiver.await.context("backend startup failed")?.connection;
        Ready { endpoint: connection.endpoint.clone(), control: None, backend: Some(connection), sync: None }
    } else {
        let (ready, receiver) = oneshot::channel();
        let release = control::release()?;
        let (path, environment) = (init.state.join("environment.sqlite"), release.deployment.environment.clone());
        let system = tokio::task::spawn_blocking(move || -> Result<_> {
            let store = chunk_store::SqliteStore::open(path, &environment)?;
            Ok(chunk_backend::Backend::new(environment, Box::new(store))?.system())
        })
        .await??;
        let config = chunk_control::server::Config {
            connection: init.state.join("connection.json"),
            state: init.state.clone(),
            system,
            listener: tokio::net::TcpListener::bind("127.0.0.1:0").await?,
            control: chunk_control::Config { environment: release.deployment.environment.clone() },
            host: Arc::new(control::SyntheticHost::new(init.backend)),
            fresh: false,
            services: None,
        };
        let token = stop.clone();
        tasks.spawn(async move { Ok(chunk_control::server::run(config, ready, token).await?) });
        let started = receiver.await.context("control startup failed")?;
        started.control.activate_release(release)?;
        let connection = started.connection;
        Ready { endpoint: connection.endpoint.clone(), control: Some(connection), backend: None, sync: None }
    };
    println!("{}", serde_json::to_string(&ready)?);
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    loop {
        tokio::select! {
            line = input.next_line() => match line?.as_deref() {
                Some("reset") => {
                    backend::reset_phases();
                    println!("null");
                }
                Some("report") => println!("{}", backend::report_phases(&init.output)?),
                _ => break,
            },
            result = tasks.join_next() => {
                result.context("missing target task")???;
                anyhow::bail!("target stopped before shutdown");
            }
        }
    }
    stop.cancel();
    while let Some(result) = tasks.join_next().await {
        result??;
    }
    Ok(())
}
