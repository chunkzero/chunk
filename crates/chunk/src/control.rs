use std::{
    collections::BTreeMap,
    io::{self, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use chunk_control::{Config, Control, ControlConnection, MachineProfile, ProcessHost, SessionType};
use chunk_proto::v1::{DeploymentRef, local_control_server::LocalControlServer};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;

#[derive(clap::Args)]
pub(crate) struct Options {
    #[arg(long, default_value = ".chunk/control")]
    state: PathBuf,
    #[arg(long, default_value = ".chunk/control.json")]
    connection: PathBuf,
    #[arg(long, default_value = "127.0.0.1:25567")]
    bind: SocketAddr,
    #[arg(long, default_value = "jvm/runtime/build/install/runtime")]
    distribution: PathBuf,
    #[arg(long, default_value = "java")]
    java: PathBuf,
    #[arg(long, default_value = "local")]
    environment: String,
    #[arg(long, default_value = "local")]
    deployment: String,
    #[arg(long, default_value = "local-bridge-fixture")]
    artifact_digest: String,
}

pub(crate) async fn run(options: Options) -> io::Result<()> {
    if !options.bind.ip().is_loopback() {
        return Err(io::Error::other("control must bind loopback"));
    }
    std::fs::create_dir_all(&options.state)?;
    let control = open_control(&options)?;
    let token = secret(&options.state.join("token"))?;
    let listener = TcpListener::bind(options.bind).await?;
    let connection = ControlConnection {
        endpoint: format!("http://{}", listener.local_addr()?),
        token: token.clone(),
    };
    if let Ok(bytes) = std::fs::read(&options.connection) {
        let old: ControlConnection = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if old.token != token {
            return Err(io::Error::other("connection file belongs to another control authority"));
        }
    }
    let shutdown = CancellationToken::new();
    let service = chunk_control::Service::new(control.clone(), token).map_err(io::Error::other)?;
    let stop_server = shutdown.clone();
    let mut server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(
                LocalControlServer::new(service)
                    .max_decoding_message_size(65_536)
                    .max_encoding_message_size(8 * 1024 * 1024),
            )
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), stop_server.cancelled_owned())
            .await
    });
    let stop_reconcile = shutdown.clone();
    let reconciler = control.clone();
    let mut reconcile = tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                () = stop_reconcile.cancelled() => break,
                _ = timer.tick() => {
                    if let Err(error) = reconciler.reconcile_all().await { tracing::warn!(%error, "control reconciliation unavailable"); }
                }
            }
        }
    });
    let bytes = serde_json::to_vec(&connection).map_err(io::Error::other)?;
    let result = async {
        if let Some(parent) = options.connection.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = options
            .connection
            .with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        private_file(&temporary)?.write_all(&bytes)?;
        std::fs::rename(temporary, &options.connection)?;
        tracing::info!(endpoint = %connection.endpoint, "local control ready");
        tokio::select! {
            result = crate::shutdown_signal()? => result,
            _ = &mut server => Err(io::Error::other("control server stopped")),
        }
    }
    .await;
    shutdown.cancel();
    if tokio::time::timeout(Duration::from_secs(3), &mut reconcile)
        .await
        .is_err()
    {
        reconcile.abort();
        let _ = reconcile.await;
    }
    if !server.is_finished() && tokio::time::timeout(Duration::from_secs(5), &mut server).await.is_err() {
        server.abort();
        let _ = server.await;
    }
    let stopped = tokio::time::timeout(Duration::from_secs(45), control.shutdown()).await;
    if std::fs::read(&options.connection).ok().as_deref() == Some(&bytes) {
        std::fs::remove_file(options.connection)?;
    }
    stopped.map_err(io::Error::other)?.map_err(io::Error::other)?;
    result
}

fn open_control(options: &Options) -> io::Result<Arc<Control>> {
    let deployment = DeploymentRef {
        environment: options.environment.clone(),
        deployment: options.deployment.clone(),
    };
    let profiles = BTreeMap::from([(
        "local".into(),
        MachineProfile {
            memory_mib: 512,
            max_sessions: 4,
        },
    )]);
    let config = Config {
        deployment: deployment.clone(),
        artifact_digest: options.artifact_digest.clone(),
        profiles: profiles.clone(),
        session_types: BTreeMap::from([(
            "bridge".into(),
            SessionType {
                machine_profile: "local".into(),
                capacity: 16,
            },
        )]),
        max_processes: 4,
    };
    let host = Arc::new(ProcessHost {
        program: std::env::current_exe()?,
        distribution: options.distribution.canonicalize()?,
        java: options.java.clone(),
        directory: options.state.join("runtimes"),
        deployment,
        artifact_digest: options.artifact_digest.clone(),
        profiles,
    });
    let control = Control::open(&options.state.join("directory.sqlite"), config, host).map_err(io::Error::other)?;
    Ok(control)
}

fn secret(path: &Path) -> io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(token) => Ok(token),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
            private_file(path)?.write_all(token.as_bytes())?;
            Ok(token)
        }
        Err(error) => Err(error),
    }
}

fn private_file(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
