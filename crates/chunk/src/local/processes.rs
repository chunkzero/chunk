use std::{
    collections::{BTreeSet, VecDeque},
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime},
};

use serde::de::DeserializeOwned;
use tokio::{
    process::{Child, Command},
    task::JoinSet,
    time::{Instant, sleep, timeout},
};

use super::{Artifact, Options, Project};

struct Process {
    child: Child,
    started: SystemTime,
    name: &'static str,
    pid: u32,
    pid_file: PathBuf,
}

impl Process {
    fn spawn(program: &Path, name: &'static str, arguments: &[OsString], directory: &Path) -> io::Result<Self> {
        let mut log = fs::File::options();
        log.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            log.mode(0o600);
        }
        let log = log.open(directory.join(format!("{name}.log")))?;
        let started = SystemTime::now();
        let child = Command::new(program)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .kill_on_drop(true)
            .spawn()?;
        tracing::info!(service = name, pid = child.id(), "started local service");
        let pid = child.id().ok_or_else(|| io::Error::other("missing child PID"))?;
        let pid_file = directory.join(format!("{name}.pid"));
        fs::write(&pid_file, pid.to_string())?;
        Ok(Self {
            child,
            started,
            name,
            pid,
            pid_file,
        })
    }

    async fn record<T: DeserializeOwned>(&mut self, path: &Path) -> io::Result<T> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if self.child.try_wait()?.is_some() {
                return Err(io::Error::other(format!("{} failed; inspect its log", self.name)));
            }
            if fs::metadata(path)
                .and_then(|m| m.modified())
                .is_ok_and(|t| t >= self.started)
                && let Ok(bytes) = fs::read(path)
                && let Ok(record) = serde_json::from_slice(&bytes)
            {
                return Ok(record);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(format!("{} readiness timed out", self.name)));
            }
            sleep(Duration::from_millis(25)).await;
        }
    }

    async fn stop(&mut self, seconds: u64) -> io::Result<()> {
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        #[cfg(unix)]
        {
            if let Some(pid) = self.child.id() {
                Command::new("kill").args(["-TERM", &pid.to_string()]).status().await?;
            }
        }
        #[cfg(not(unix))]
        self.child.start_kill()?;
        if let Ok(result) = timeout(Duration::from_secs(seconds), self.child.wait()).await {
            result?;
        } else {
            self.child.start_kill()?;
            self.child.wait().await?;
        }
        Ok(())
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if fs::read_to_string(&self.pid_file).is_ok_and(|pid| pid == self.pid.to_string()) {
            let _ = fs::remove_file(&self.pid_file);
        }
    }
}

pub(super) struct Services {
    program: PathBuf,
    directory: PathBuf,
    runtimes: PathBuf,
    backend_args: Vec<OsString>,
    control_args: Vec<OsString>,
    edge_args: Vec<OsString>,
    backend: Option<Process>,
    control: Option<Process>,
    edge: Option<Process>,
    restarts: VecDeque<Instant>,
    backend_identity: Option<chunk_contract::BackendConnection>,
    control_identity: Option<chunk_contract::ControlConnection>,
    bind: std::net::SocketAddr,
}

impl Services {
    pub fn new(options: &Options, project: &Project, artifact: &Artifact, program: PathBuf) -> Self {
        let directory = options.state.clone();
        let control_state = directory.join("control").join(&artifact.id);
        let backend_file = directory.join("backend.json");
        let control_file = directory.join("control.json");
        let mut backend_args = vec!["backend".into()];
        argument(&mut backend_args, "--bundle", artifact.directory.join("backend.json"));
        argument(&mut backend_args, "--environment", &project.environment);
        argument(&mut backend_args, "--state", directory.join("backend"));
        argument(&mut backend_args, "--connection", &backend_file);
        argument(&mut backend_args, "--bind", options.backend_bind.to_string());
        let mut control_args = vec!["control".into()];
        argument(&mut control_args, "--state", &control_state);
        argument(&mut control_args, "--connection", &control_file);
        argument(&mut control_args, "--bind", options.control_bind.to_string());
        argument(&mut control_args, "--distribution", artifact.directory.join("gameplay"));
        argument(&mut control_args, "--java", &options.java);
        argument(&mut control_args, "--config", directory.join("control-config.json"));
        argument(&mut control_args, "--backend-file", &backend_file);
        let mut edge_args = vec!["edge".into()];
        argument(&mut edge_args, "--bind", options.bind.to_string());
        argument(&mut edge_args, "--backend-file", backend_file);
        argument(&mut edge_args, "--control-file", control_file);
        Self {
            program,
            directory,
            runtimes: control_state.join("runtimes"),
            backend_args,
            control_args,
            edge_args,
            backend: None,
            control: None,
            edge: None,
            restarts: VecDeque::new(),
            backend_identity: None,
            control_identity: None,
            bind: options.bind,
        }
    }

    pub async fn start(&mut self) -> io::Result<()> {
        self.start_backend().await?;
        self.start_control().await?;
        self.edge = Some(Process::spawn(&self.program, "edge", &self.edge_args, &self.directory)?);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if self
                .edge
                .as_mut()
                .ok_or_else(|| io::Error::other("edge missing"))?
                .child
                .try_wait()?
                .is_some()
            {
                return Err(io::Error::other("edge failed; inspect edge.log"));
            }
            if tokio::net::TcpStream::connect(self.bind).await.is_ok() {
                break;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other("edge readiness timed out"));
            }
            sleep(Duration::from_millis(25)).await;
        }
        Ok(())
    }

    async fn start_backend(&mut self) -> io::Result<()> {
        self.backend = Some(Process::spawn(
            &self.program,
            "backend",
            &self.backend_args,
            &self.directory,
        )?);
        let connection: chunk_contract::BackendConnection = self
            .backend
            .as_mut()
            .ok_or_else(|| io::Error::other("backend missing"))?
            .record(&self.directory.join("backend.json"))
            .await?;
        if let Some(previous) = &self.backend_identity
            && (previous.endpoint != connection.endpoint
                || previous.token != connection.token
                || previous.environment != connection.environment
                || previous.deployment != connection.deployment)
        {
            return Err(io::Error::other("backend restart changed its identity"));
        }
        self.backend_identity = Some(connection);
        Ok(())
    }

    async fn start_control(&mut self) -> io::Result<()> {
        self.control = Some(Process::spawn(
            &self.program,
            "control",
            &self.control_args,
            &self.directory,
        )?);
        let connection: chunk_contract::ControlConnection = self
            .control
            .as_mut()
            .ok_or_else(|| io::Error::other("control missing"))?
            .record(&self.directory.join("control.json"))
            .await?;
        if let Some(previous) = &self.control_identity
            && (previous.endpoint != connection.endpoint || previous.token != connection.token)
        {
            return Err(io::Error::other("control restart changed its identity"));
        }
        self.control_identity = Some(connection);
        Ok(())
    }

    pub async fn poll(&mut self) -> io::Result<()> {
        if let Some(edge) = &mut self.edge
            && edge.child.try_wait()?.is_some()
        {
            return Err(io::Error::other("edge stopped; inspect edge.log"));
        }
        if let Some(backend) = &mut self.backend
            && let Some(status) = backend.child.try_wait()?
        {
            tracing::warn!(%status, "backend stopped; restarting the same deployment and database");
            self.restart_delay().await?;
            self.start_backend().await?;
        }
        if let Some(control) = &mut self.control
            && let Some(status) = control.child.try_wait()?
        {
            tracing::warn!(%status, "control stopped; recovering the same ownership authority");
            self.restart_delay().await?;
            self.start_control().await?;
        }
        Ok(())
    }

    async fn restart_delay(&mut self) -> io::Result<()> {
        let now = Instant::now();
        self.restarts
            .retain(|at| now.duration_since(*at) < Duration::from_secs(60));
        if self.restarts.len() >= 3 {
            return Err(io::Error::other("local services repeatedly failed; inspect their logs"));
        }
        self.restarts.push_back(now);
        sleep(Duration::from_secs(2)).await;
        Ok(())
    }

    pub async fn stop(&mut self) -> io::Result<()> {
        let mut result = Ok(());
        for (process, seconds) in [(&mut self.edge, 5), (&mut self.control, 50)] {
            if let Some(process) = process
                && let Err(error) = process.stop(seconds).await
            {
                result = Err(error);
            }
        }
        // Control can fail before its own cleanup runs. Its durable runtime records still authorize shutdown.
        if let Err(error) = stop_runtimes(&self.runtimes).await {
            result = Err(error);
        }
        if let Some(backend) = &mut self.backend
            && let Err(error) = backend.stop(5).await
        {
            result = Err(error);
        }
        result
    }
}

fn argument(arguments: &mut Vec<OsString>, name: &str, value: impl AsRef<std::ffi::OsStr>) {
    arguments.push(name.into());
    arguments.push(value.as_ref().to_owned());
}

async fn stop_runtimes(directory: &Path) -> io::Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    let mut records = BTreeSet::new();
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "json" && e != "launch") || path.with_extension("exit").exists() {
            continue;
        }
        records.insert(path.with_extension("json"));
    }
    let mut tasks = JoinSet::new();
    for path in records {
        tasks.spawn(async move {
            let result = timeout(Duration::from_secs(75), stop_runtime(&path))
                .await
                .map_err(io::Error::other)
                .and_then(|r| r);
            if path.with_extension("exit").exists() {
                Ok(())
            } else {
                result.map_err(|error| io::Error::other(format!("{}: {error}", path.display())))
            }
        });
    }
    let mut result = Ok(());
    while let Some(stopped) = tasks.join_next().await {
        if let Err(error) = stopped.map_err(io::Error::other).and_then(|r| r) {
            result = Err(error);
        }
    }
    result
}

async fn stop_runtime(path: &Path) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(35);
    let runtime: chunk_runtime::RuntimeConnection = loop {
        if path.with_extension("exit").exists() {
            return Ok(());
        }
        if let Ok(bytes) = fs::read(path)
            && let Ok(record) = serde_json::from_slice(&bytes)
        {
            break record;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("runtime launch is unresolved"));
        }
        sleep(Duration::from_millis(25)).await;
    };
    let address: std::net::SocketAddr = runtime
        .endpoint
        .strip_prefix("http://")
        .ok_or_else(|| io::Error::other("runtime URL"))?
        .parse()
        .map_err(io::Error::other)?;
    if !address.ip().is_loopback() {
        return Err(io::Error::other("runtime requires loopback"));
    }
    let channel = tonic::transport::Channel::from_shared(runtime.endpoint)
        .map_err(io::Error::other)?
        .connect_timeout(Duration::from_secs(3))
        .connect()
        .await
        .map_err(io::Error::other)?;
    let mut request = tonic::Request::new(runtime.identity);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", runtime.token).parse().map_err(io::Error::other)?,
    );
    request.set_timeout(Duration::from_secs(10));
    let _ = chunk_proto::v1::process_control_client::ProcessControlClient::new(channel)
        .stop_process(request)
        .await;
    let deadline = Instant::now() + Duration::from_secs(20);
    while !path.with_extension("exit").exists() {
        if Instant::now() >= deadline {
            return Err(io::Error::other("runtime shutdown is unresolved"));
        }
        sleep(Duration::from_millis(25)).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
