//! Small process adapters shared by standalone service binaries.

mod connections;

pub use connections::{Closable, Connections, GRACE};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    str::FromStr,
};
use tokio_util::sync::CancellationToken;

/// Reads a required service environment variable.
/// # Errors
/// Reports missing, non-Unicode or invalid values without exposing their contents.
pub fn required<T: FromStr>(name: &str) -> io::Result<T> {
    std::env::var(name)
        .map_err(|_| io::Error::other(format!("{name} is required")))?
        .parse()
        .map_err(|_| io::Error::other(format!("invalid {name}")))
}

/// Reads an optional service environment variable.
/// # Errors
/// Reports invalid or non-Unicode values.
pub fn optional<T: FromStr>(name: &str) -> io::Result<Option<T>> {
    match std::env::var(name) {
        Ok(value) => value.parse().map(Some).map_err(|_| io::Error::other(format!("invalid {name}"))),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err(io::Error::other(format!("invalid {name}"))),
    }
}

/// Reads a JSON configuration file.
/// # Errors
/// Reports I/O and decoding errors.
pub fn read<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)
}

/// Creates a private file without replacing an existing owner.
/// # Errors
/// Reports I/O errors, including an existing file.
pub fn private_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Whether `presented` equals `expected`, in time that depends only on their lengths.
#[must_use]
pub fn same_secret(presented: &str, expected: &str) -> bool {
    presented.len() == expected.len()
        && presented
            .bytes()
            .zip(expected.bytes())
            .fold(0, |difference, (a, b)| difference | std::hint::black_box(a ^ b))
            == 0
}

/// Loads or creates a durable credential.
/// # Errors
/// Reports I/O errors.
pub fn secret(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(value) => Ok(value),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let value = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
            private_file(path)?.write_all(value.as_bytes())?;
            Ok(value)
        }
        Err(error) => Err(error),
    }
}

/// A published discovery record, removed only while its contents still match.
pub struct Record {
    path: PathBuf,
    bytes: Vec<u8>,
}
impl Record {
    /// Atomically publishes a private JSON discovery record.
    /// # Errors
    /// Reports serialization and filesystem errors.
    pub fn publish(path: &Path, value: &impl Serialize) -> io::Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
        let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            private_file(&temporary)?.write_all(&bytes)?;
            fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result?;
        Ok(Self { path: path.to_owned(), bytes })
    }
}
impl Drop for Record {
    fn drop(&mut self) {
        if fs::read(&self.path).ok().as_deref() == Some(&self.bytes) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Installs process-wide logging. Call once, from the executable.
pub fn logging() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
}

/// Runs a service until an OS shutdown signal, then awaits its cleanup.
/// # Errors
/// Reports signal registration or service errors.
pub async fn run<F, Fut>(service: F) -> io::Result<()>
where
    F: FnOnce(CancellationToken) -> Fut,
    Fut: Future<Output = io::Result<()>>,
{
    let shutdown = shutdown_signal()?;
    let stop = CancellationToken::new();
    let running = service(stop.clone());
    tokio::pin!(running);
    tokio::select! {
        result = &mut running => result,
        result = shutdown => {
            stop.cancel();
            let stopped = running.await;
            result.and(stopped)
        }
    }
}

/// Registers shutdown signals before startup begins.
/// # Errors
/// Reports OS signal registration errors.
pub fn shutdown_signal() -> io::Result<impl Future<Output = io::Result<()>>> {
    #[cfg(unix)]
    let wait = {
        use tokio::signal::unix::{SignalKind, signal};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        async move {
            tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
        }
    };
    #[cfg(not(unix))]
    let wait = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    Ok(async move {
        wait.await;
        Ok(())
    })
}
