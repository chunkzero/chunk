use super::*;
use std::path::Path;

/// A stand-in for Java that runs `script` in `sh`.
fn java(script: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", script]);
    command
}

async fn until_exists(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(Instant::now() < deadline, "{} was never created", path.display());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn signals_are_forwarded_and_the_jvms_exit_code_passes_through() {
    let directory = tempfile::tempdir().unwrap();
    let (ready, quit) = (directory.path().join("ready"), directory.path().join("quit"));
    let mut command = java(&format!(
        "trap 'touch {quit}' QUIT; trap 'exit 7' TERM; touch {ready}; while :; do sleep 0.05; done",
        quit = quit.display(),
        ready = ready.display()
    ));
    let (sender, mut signals) = mpsc::unbounded_channel();
    let supervised = tokio::spawn(async move { run(&mut command, &mut signals, Duration::from_secs(30)).await });
    until_exists(&ready).await;
    sender.send(Signal::QUIT).unwrap();
    until_exists(&quit).await;
    assert!(!supervised.is_finished());
    sender.send(Signal::TERM).unwrap();
    assert_eq!(supervised.await.unwrap().unwrap(), 7);
}

#[tokio::test]
async fn a_jvm_that_the_forwarded_stop_ends_exits_cleanly() {
    let (sender, mut signals) = mpsc::unbounded_channel();
    let mut command = java("exec sleep 30");
    let supervised = tokio::spawn(async move { run(&mut command, &mut signals, Duration::from_secs(30)).await });
    sender.send(Signal::TERM).unwrap();
    assert_eq!(supervised.await.unwrap().unwrap(), 0);
}

#[tokio::test]
async fn a_jvm_that_outlives_its_stop_grace_is_killed() {
    let directory = tempfile::tempdir().unwrap();
    let ready = directory.path().join("ready");
    let mut command = java(&format!("trap '' TERM INT; touch {}; while :; do sleep 0.05; done", ready.display()));
    let (sender, mut signals) = mpsc::unbounded_channel();
    let grace = Duration::from_millis(300);
    let supervised = tokio::spawn(async move { run(&mut command, &mut signals, grace).await });
    until_exists(&ready).await;
    let stopping = Instant::now();
    sender.send(Signal::INT).unwrap();
    assert_eq!(supervised.await.unwrap().unwrap(), 128 + Signal::KILL.as_raw());
    assert!(stopping.elapsed() >= grace);
}

#[tokio::test]
async fn exit_codes_and_fatal_signals_pass_through() {
    let (_sender, mut signals) = mpsc::unbounded_channel();
    for (script, code) in [("exit 0", 0), ("exit 42", 42), ("kill -KILL $$", 137)] {
        assert_eq!(run(&mut java(script), &mut signals, Duration::from_secs(1)).await.unwrap(), code, "{script}");
    }
    let missing = run(&mut Command::new("/nonexistent/java"), &mut signals, Duration::from_secs(1)).await;
    assert_eq!(missing.unwrap_err().code, 78);
}
