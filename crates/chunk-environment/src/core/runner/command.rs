use super::{LaunchSpec, Launcher};
use std::{io, process::Stdio};

/// Starts each host's machine by running `launch`, and stops it by running `release`, such as scripts or `podman`
/// commands. Each runs with `CHUNK_HOST_ID` set, and `launch` with the runner's environment and `CHUNK_MEMORY_MIB` too.
/// `launch` must exit once the machine has started, and `release` exit successfully only once no machine for the host
/// runs, including one never launched or whose `launch` was killed. A command core stops waiting for, past the host's
/// readiness deadline or its release timeout, is killed, though not the processes it started.
pub struct CommandLauncher {
    /// The program and its arguments.
    pub launch: Vec<String>,
    pub release: Vec<String>,
}

#[tonic::async_trait]
impl Launcher for CommandLauncher {
    async fn launch(&self, id: &str, credential: &str, spec: &LaunchSpec) -> io::Result<()> {
        let mut env = spec.env(credential);
        env.push(("CHUNK_MEMORY_MIB", spec.memory_mib.to_string()));
        run(&self.launch, id, env).await
    }

    async fn release(&self, id: &str) -> io::Result<bool> {
        run(&self.release, id, Vec::new()).await.map(|()| true)
    }
}

async fn run(command: &[String], id: &str, env: Vec<(&str, String)>) -> io::Result<()> {
    let (program, arguments) = command.split_first().ok_or_else(|| io::Error::other("the command is empty"))?;
    let status = tokio::process::Command::new(program)
        .args(arguments)
        .env("CHUNK_HOST_ID", id)
        .envs(env)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await?;
    if status.success() { Ok(()) } else { Err(io::Error::other(format!("{program} exited with {status}"))) }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn commands_run_with_the_runners_environment() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("env");
        let script = r#"printf '%s %s %s %s' "$CHUNK_HOST_ID" "$CHUNK_JVM_CREDENTIAL" "$CHUNK_MEMORY_MIB" "$CHUNK_PLAYER_ADDRESS" > "$0""#;
        let sh = |script: &str| vec!["sh".to_owned(), "-c".to_owned(), script.to_owned(), output.display().to_string()];
        let launcher = CommandLauncher { launch: sh(script), release: sh("exit 3") };
        let spec = LaunchSpec {
            core_endpoint: "http://10.0.0.2:4000".into(),
            environment: "test".into(),
            player_address: Some("10.0.0.3".parse().unwrap()),
            memory_mib: 512,
        };
        launcher.launch("host-1", "credential", &spec).await.unwrap();
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "host-1 credential 512 10.0.0.3");
        assert!(launcher.release("host-1").await.is_err());
        assert!(CommandLauncher { launch: Vec::new(), release: sh("true") }.release("host-1").await.unwrap());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_command_core_stops_waiting_for_is_killed() {
        let directory = tempfile::tempdir().unwrap();
        let pid = directory.path().join("pid");
        let script =
            vec!["sh".into(), "-c".into(), r#"echo $$ > "$0.tmp" && mv "$0.tmp" "$0" && exec sleep 60"#.into()];
        let launcher =
            CommandLauncher { launch: Vec::new(), release: [script, vec![pid.display().to_string()]].concat() };
        let started = async {
            while !pid.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::select! {
            released = launcher.release("host-1") => panic!("the command exited: {released:?}"),
            () = started => {}
        }
        let pid = std::fs::read_to_string(&pid).unwrap();
        // The process is gone, or a zombie until the runtime reaps it.
        let killed = async {
            while std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()))
                .is_ok_and(|stat| stat.rsplit_once(") ").is_some_and(|(_, state)| !state.starts_with('Z')))
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), killed).await.expect("the command was killed");
    }
}
