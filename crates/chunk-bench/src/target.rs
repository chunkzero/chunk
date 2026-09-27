use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    process::{Child, ChildStdin, ChildStdout, Command},
};
use tokio_util::sync::CancellationToken;

use crate::{
    backend,
    config::{Config, Scenario},
    control, fixtures, proxy, sync,
};

#[derive(Serialize, Deserialize)]
pub struct Init {
    pub config: Config,
    /// The synthetic gameplay server's address, the compiled deployment path for backend workloads, or empty.
    pub backend: String,
    pub state: PathBuf,
    pub output: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub struct Ready {
    pub endpoint: String,
    /// Core's, for every workload but the relay.
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
        let ready = Ready { endpoint: listener.local_addr()?.to_string(), sync: None };
        let token = stop.clone();
        let (address, config) = (init.backend.clone(), init.config.clone());
        tasks.spawn(async move { proxy::target(listener, &address, &config, token).await });
        ready
    } else {
        let core = core(&init).await?;
        let control = core.control_connection()?;
        let gateway = core.target()?.gateway;
        let connection = sync::Connection {
            endpoint: control.endpoint.clone(),
            cli: control.token.clone(),
            gateway_id: gateway.id,
            gateway: gateway.credential,
        };
        let token = stop.clone();
        tasks.spawn(async move {
            token.cancelled().await;
            Ok(core.stop(|| {}).await?)
        });
        Ready { endpoint: connection.endpoint.clone(), sync: Some(connection) }
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

/// Starts core: on the compiled bundle for backend workloads, or on synthetic hosts serving the control fixture's
/// release.
async fn core(init: &Init) -> Result<chunk_environment::Core> {
    let state = init.state.join("core");
    let config = chunk_environment::CoreConfig {
        bundle: init.config.scenario.is_backend().then(|| init.backend.clone().into()),
        environment: backend::ENVIRONMENT.into(),
        backend_record: state.join("backend.json"),
        control_record: state.join("control.json"),
        state,
        backend_bind: "127.0.0.1:0".parse()?,
        control_bind: "127.0.0.1:0".parse()?,
        fresh: false,
    };
    if init.config.scenario.is_backend() {
        chunk_backend::observe(backend::observe);
        return Ok(chunk_environment::Core::start(config, |_| {}).await?);
    }
    let host = Arc::new(fixtures::SyntheticHost::default());
    let core = chunk_environment::Core::start_with_host(config, host.clone()).await?;
    host.attach(&core.control()?);
    core.control()?.activate_release(control::release()?)?;
    Ok(core)
}
