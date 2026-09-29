use super::{LaunchSpec, Launcher};
use rustix::process::{Pid, Signal, kill_process_group};
use std::{io, process::Stdio};
use tokio_util::sync::CancellationToken;

/// Starts each host's machine by running `launch`, and stops it by running `release`, such as scripts or `podman`
/// commands. Each runs with `CHUNK_HOST_ID` set, and `launch` with the runner's environment and `CHUNK_MEMORY_MIB` too.
/// `launch` must exit once the machine has started, or once its attempt ended, and `release` exit successfully only
/// once no machine for the host runs or can start, including one never launched, one whose `launch` was killed, and
/// one an earlier core's `launch` is still starting.
///
/// Each command runs in its own process group. A `launch` core cancels, and a command core stops waiting for, is
/// killed with its whole group, so a `launch` must not detach work that could still start the machine from that group,
/// as `setsid` or a daemon would.
pub struct CommandLauncher {
    /// The program and its arguments.
    pub launch: Vec<String>,
    pub release: Vec<String>,
}

#[tonic::async_trait]
impl Launcher for CommandLauncher {
    async fn launch(
        &self,
        id: &str,
        credential: &str,
        spec: &LaunchSpec,
        cancel: &CancellationToken,
    ) -> io::Result<()> {
        let mut env = spec.env(credential);
        env.push(("CHUNK_MEMORY_MIB", spec.memory_mib.to_string()));
        run(&self.launch, id, env, cancel).await
    }

    async fn release(&self, id: &str) -> io::Result<bool> {
        run(&self.release, id, Vec::new(), &CancellationToken::new()).await.map(|()| true)
    }
}

/// Runs `command` until it exits, or, once `cancel` fires, kills its group and returns once the command exited.
async fn run(command: &[String], id: &str, env: Vec<(&str, String)>, cancel: &CancellationToken) -> io::Result<()> {
    let (program, arguments) = command.split_first().ok_or_else(|| io::Error::other("the command is empty"))?;
    let mut child = tokio::process::Command::new(program)
        .args(arguments)
        .env("CHUNK_HOST_ID", id)
        .envs(env)
        .stdin(Stdio::null())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    // Dropped before `child`, so a dropped call kills the group before the command is reaped.
    let mut group = Group(child.id().and_then(|id| Pid::from_raw(i32::try_from(id).ok()?)));
    let status = tokio::select! {
        status = child.wait() => status,
        () = cancel.cancelled() => {
            group.kill();
            child.wait().await?;
            return Err(io::Error::other(format!("{program} was cancelled")));
        }
    };
    // Once reaped, the command's ID may name another group.
    group.0 = None;
    let status = status?;
    if status.success() { Ok(()) } else { Err(io::Error::other(format!("{program} exited with {status}"))) }
}

/// A command's process group, killed when dropped until its command is reaped.
struct Group(Option<Pid>);

impl Group {
    fn kill(&mut self) {
        if let Some(group) = self.0.take() {
            let _ = kill_process_group(group, Signal::KILL);
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::Path, time::Duration};

    #[tokio::test]
    async fn commands_run_with_the_runners_environment() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("env");
        let script = r#"printf '%s %s %s %s' "$CHUNK_HOST_ID" "$CHUNK_JVM_CREDENTIAL" "$CHUNK_MEMORY_MIB" "$CHUNK_PLAYER_ADDRESS" > "$0""#;
        let launcher = CommandLauncher { launch: sh(script, &output), release: sh("exit 3", &output) };
        let spec = LaunchSpec {
            core_endpoint: "http://10.0.0.2:4000".into(),
            environment: "test".into(),
            release_id: "release-1".into(),
            app: "lobby".into(),
            profile: "small".into(),
            player_address: Some("10.0.0.3".parse().unwrap()),
            memory_mib: 512,
        };
        launcher.launch("host-1", "credential", &spec, &CancellationToken::new()).await.unwrap();
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "host-1 credential 512 10.0.0.3");
        assert!(launcher.release("host-1").await.is_err());
        let released = CommandLauncher { launch: Vec::new(), release: sh("true", &output) }.release("host-1").await;
        assert!(released.unwrap());
    }

    /// A command that forks a child, which would create the marker `$0` later, then waits.
    const FORKS: &str = r#"(sleep 1 && touch "$0") & touch "$0.started" && wait"#;

    #[tokio::test]
    async fn a_cancelled_launch_is_killed_with_its_children() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("machine");
        let launcher = CommandLauncher { launch: sh(FORKS, &marker), release: Vec::new() };
        let spec = LaunchSpec {
            core_endpoint: "http://10.0.0.2:4000".into(),
            environment: "test".into(),
            release_id: "release-1".into(),
            app: "lobby".into(),
            profile: "small".into(),
            player_address: None,
            memory_mib: 512,
        };
        let cancel = CancellationToken::new();
        let launching = launcher.launch("host-1", "credential", &spec, &cancel);
        let cancelled = async {
            started(&marker).await;
            cancel.cancel();
        };
        let ended = tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(launching, cancelled).0 });
        assert!(ended.await.expect("the launch ended once cancelled").is_err());
        never_created(&marker).await;
    }

    #[tokio::test]
    async fn a_dropped_command_is_killed_with_its_children() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("machine");
        let launcher = CommandLauncher { launch: Vec::new(), release: sh(FORKS, &marker) };
        tokio::select! {
            released = launcher.release("host-1") => panic!("the command exited: {released:?}"),
            () = started(&marker) => {}
        }
        never_created(&marker).await;
    }

    fn sh(script: &str, argument: &Path) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into(), argument.display().to_string()]
    }

    async fn started(marker: &Path) {
        while !marker.with_extension("started").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn never_created(marker: &Path) {
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!marker.exists(), "the command's child outlived it");
    }
}
