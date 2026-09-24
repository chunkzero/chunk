use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use chunk_control::ControlConnection;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    process::{Child, ChildStdin, Command},
    sync::oneshot,
};
use tokio_util::sync::CancellationToken;

use crate::{
    config::{Config, Scenario},
    control, proxy,
};

#[derive(Serialize, Deserialize)]
pub struct Init {
    pub config: Config,
    pub backend: String,
    pub state: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub struct Ready {
    pub endpoint: String,
    pub control: Option<ControlConnection>,
}

pub struct Target {
    child: Child,
    stdin: ChildStdin,
    pub ready: Ready,
}

impl Target {
    pub async fn start(config: &Config, backend: String, state: &Path) -> Result<Self> {
        let mut child = Command::new(std::env::current_exe()?)
            .arg("--worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdin = child.stdin.take().context("target stdin")?;
        let init = Init { config: config.clone(), backend, state: state.into() };
        stdin.write_all(format!("{}\n", serde_json::to_string(&init)?).as_bytes()).await?;
        let mut stdout = BufReader::new(child.stdout.take().context("target stdout")?);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(30), stdout.read_line(&mut line)).await??;
        let ready = serde_json::from_str(&line).context("target exited without readiness")?;
        Ok(Self { child, stdin, ready })
    }

    pub fn pid(&self) -> Result<u32> {
        self.child.id().context("target already exited")
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
        let ready = Ready { endpoint: listener.local_addr()?.to_string(), control: None };
        let token = stop.clone();
        tasks.spawn(async move { proxy::target(listener, &init.backend, &init.config, token).await });
        ready
    } else {
        let (ready, receiver) = oneshot::channel();
        let config = chunk_control::server::Config {
            connection: init.state.join("connection.json"),
            state: init.state,
            bind: "127.0.0.1:0".parse()?,
            control: control::configuration()?,
            host: Arc::new(control::SyntheticHost::new(init.backend)),
        };
        let token = stop.clone();
        tasks.spawn(async move { Ok(chunk_control::server::run(config, ready, token).await?) });
        let connection = receiver.await.context("control startup failed")?;
        Ready { endpoint: connection.endpoint.clone(), control: Some(connection) }
    };
    println!("{}", serde_json::to_string(&ready)?);
    let mut input = BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    tokio::select! {
        result = input.read_line(&mut line) => { result?; }
        result = tasks.join_next() => {
            result.context("missing target task")???;
            anyhow::bail!("target stopped before shutdown");
        }
    }
    stop.cancel();
    while let Some(result) = tasks.join_next().await {
        result??;
    }
    Ok(())
}
