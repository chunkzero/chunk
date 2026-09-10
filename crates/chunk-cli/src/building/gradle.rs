use std::{ffi::OsString, fs, io, path::Path, process::Stdio, time::Duration};

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use tokio_util::sync::CancellationToken;

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

pub(super) async fn run(project: &Path, executable: &Path, stop: &CancellationToken) -> io::Result<()> {
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
        command
            .current_dir(project)
            .args(["chunkArtifacts", "--no-daemon", "--console=plain"])
            .arg(property)
            .stdin(Stdio::null());
    });
    command.wrap(KillOnDrop);
    #[cfg(unix)]
    command.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(process_wrap::tokio::JobObject);
    let child = command.spawn().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("could not start Gradle wrapper {}: {error}", wrapper.display()),
        )
    })?;
    let mut process = BuildProcess { child, finished: false };
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
    if !status.success() {
        return Err(io::Error::other(format!("Gradle chunkArtifacts failed with {status}")));
    }
    process.finished = true;
    Ok(())
}
