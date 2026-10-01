use std::{ffi::OsString, fs, io, path::Path, process::Stdio, sync::Arc, time::Duration};

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use tokio_util::sync::CancellationToken;

use super::BuildMode;
use super::output::Capture;
use super::progress::Progress;

struct BuildProcess {
    child: Box<dyn ChildWrapper>,
    finished: bool,
}

impl Drop for BuildProcess {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.child.start_kill();
        }
    }
}

/// Captures `chunkArtifacts` output, forwarding it to an optional progress observer.
pub(super) async fn run(
    project: &Path,
    executable: &Path,
    mode: BuildMode,
    stop: &CancellationToken,
    progress: &Progress,
) -> io::Result<()> {
    super::cancelled(stop)?;
    let wrapper = project.join(if cfg!(windows) { "gradlew.bat" } else { "gradlew" });
    if !fs::metadata(&wrapper).is_ok_and(|metadata| metadata.is_file()) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("project Gradle wrapper missing: {}", wrapper.display()),
        ));
    }
    let mut property = OsString::from("-Pchunk.executable=");
    property.push(executable);
    let mut command = CommandWrap::with_new(&wrapper, |command| {
        chunk_service::withhold_platform_env(command.as_std_mut());
        command
            .current_dir(project)
            .arg("chunkArtifacts")
            .args(match mode {
                BuildMode::Release => &["--no-daemon"][..],
                BuildMode::Dev => &["-Dorg.gradle.daemon.idletimeout=900000", "-Pchunk.dev=true"],
            })
            .arg("--console=plain")
            .arg(property)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    });
    command.wrap(KillOnDrop);
    #[cfg(unix)]
    command.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(process_wrap::tokio::JobObject);
    let child = command.spawn().map_err(|error| {
        io::Error::new(error.kind(), format!("could not start Gradle wrapper {}: {error}", wrapper.display()))
    })?;
    let mut process = BuildProcess { child, finished: false };
    let capture = Arc::new(Capture::default());
    let mut readers = tokio::task::JoinSet::new();
    if let Some(stdout) = process.child.stdout().take() {
        let capture = capture.clone();
        let progress = progress.clone();
        readers.spawn(async move { capture.read(stdout, &progress).await });
    }
    if let Some(stderr) = process.child.stderr().take() {
        let capture = capture.clone();
        let progress = progress.clone();
        readers.spawn(async move { capture.read(stderr, &progress).await });
    }
    let status = tokio::select! {
        biased;
        () = stop.cancelled() => {
            process.child.start_kill()?;
            tokio::time::timeout(Duration::from_secs(5), process.child.wait()).await.map_err(io::Error::other)??;
            process.finished = true;
            return super::cancelled(stop);
        }
        status = process.child.wait() => status?,
    };
    if status.success() {
        process.finished = true;
        // Drain the final output without waiting indefinitely on inherited pipes.
        let _ = tokio::time::timeout(Duration::from_secs(1), readers.join_all()).await;
        return Ok(());
    }
    // Stopping the group first keeps orphaned descendants from holding the pipes open.
    drop(process);
    let _ = tokio::time::timeout(Duration::from_secs(1), readers.join_all()).await;
    let excerpt = capture.excerpt();
    let mut message = format!("Gradle chunkArtifacts failed with {status}");
    if !excerpt.is_empty() {
        message.push_str("\n\n");
        message.push_str(&excerpt);
    }
    Err(io::Error::other(message))
}
