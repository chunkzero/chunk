#![cfg(unix)]

use super::*;
use std::{
    fs,
    io::{Cursor, Write},
    os::unix::fs::PermissionsExt,
    time::Duration,
};

use serde_json::json;
use zip::{ZipWriter, write::SimpleFileOptions};

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("consumer with spaces");
        fs::create_dir_all(root.join("apps/lobby")).unwrap();
        let root = root.canonicalize().unwrap();
        fs::create_dir(root.join("fixture")).unwrap();
        for name in ["chunk.toml", "apps/lobby/app.toml", "apps/lobby/build.gradle.kts"] {
            fs::write(root.join(name), "").unwrap();
        }
        let mut jar = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        for (name, bytes) in [
            ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\r\nMain-Class: sample.Provider\r\n\r\n".as_slice()),
            ("sample/Provider.class", [0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 69].as_slice()),
        ] {
            jar.start_file(name, options).unwrap();
            jar.write_all(bytes).unwrap();
        }
        fs::write(root.join("fixture/lobby.jar"), jar.finish().unwrap().into_inner()).unwrap();
        fs::write(root.join("fixture/source.mjs"), "export const value = 1;").unwrap();
        fs::write(
            root.join("fixture/contract.json"),
            br#"{"contract_version":3,"runtime_profile":"transactional_v1","tables":{},"functions":{}}"#,
        )
        .unwrap();
        fs::write(
            root.join("fixture/artifacts.json"),
            serde_json::to_vec(&json!({
                "version":4,"java":{"version":25,"executable":root.join("jdk/bin/java")},
                "apps":[{"id":"lobby","jar":root.join("fixture/lobby.jar"),"classpath":[],"java_version":25,"sessions":["default"]}]
            }))
            .unwrap(),
        )
        .unwrap();
        let fixture = Self { _directory: directory, root };
        fixture.wrapper("printf '%s\\n' \"$PWD\" \"$@\" > wrapper-arguments.txt\nmkdir -p .chunk/build/backend .chunk/build/jvm\ncp fixture/source.mjs fixture/contract.json .chunk/build/backend/\ncp fixture/artifacts.json .chunk/build/jvm/artifacts.json\n");
        fixture
    }

    fn wrapper(&self, source: &str) {
        let path = self.root.join("gradlew");
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{source}")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn project(&self) -> Project {
        prepare(&Options { project: self.root.clone(), output: None }).unwrap()
    }
}

#[tokio::test]
async fn project_build_packages_only_after_the_requested_gradle_task_finishes() {
    let fixture = Fixture::new();
    let project = fixture.project();
    assert!(!project.output.exists());
    assert!(!fixture.root.join(".chunk/build/backend").exists());
    let built = execute(&project, BuildMode::Release, CancellationToken::new(), Progress::default()).await.unwrap();
    let arguments = fs::read_to_string(fixture.root.join("wrapper-arguments.txt")).unwrap();
    assert_eq!(
        arguments.lines().collect::<Vec<_>>(),
        [
            fixture.root.to_str().unwrap(),
            "chunkArtifacts",
            "--no-daemon",
            "--console=plain",
            &format!("-Pchunk.executable={}", std::env::current_exe().unwrap().display()),
        ]
    );
    execute(&project, BuildMode::Dev, CancellationToken::new(), Progress::default()).await.unwrap();
    let arguments = fs::read_to_string(fixture.root.join("wrapper-arguments.txt")).unwrap();
    assert_eq!(
        arguments.lines().skip(2).take(2).collect::<Vec<_>>(),
        ["-Dorg.gradle.daemon.idletimeout=900000", "-Pchunk.dev=true"]
    );
    let archive = built.release.archive.unwrap();
    assert_eq!(archive.parent().unwrap(), fixture.root.join("dist"));
    assert!(archive.is_file());
    assert!(built.release.directory.join("backend.json").is_file());
    assert_eq!(built.java.version, 25);
    assert_eq!(built.java.executable, fixture.root.join("jdk/bin/java"));
    let explicit =
        prepare(&Options { project: fixture.root.clone(), output: Some("target/consumer-releases".into()) }).unwrap();
    assert_eq!(explicit.output, std::env::current_dir().unwrap().join("target/consumer-releases"));
    assert!(
        prepare(&Options { project: fixture.root.clone(), output: Some(fixture.root.join(".chunk/build")) }).is_err()
    );
}

#[tokio::test]
async fn invalid_metadata_wrapper_failure_and_missing_descriptors_do_not_publish() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("chunk.toml"), "domains=[]").unwrap();
    assert!(
        prepare(&Options { project: fixture.root.clone(), output: None })
            .err()
            .unwrap()
            .to_string()
            .contains("unknown field")
    );
    assert!(!fixture.root.join("wrapper-arguments.txt").exists());
    fs::write(fixture.root.join("chunk.toml"), "").unwrap();
    fixture.wrapper("(sleep 0.3; printf leaked > failed-leak) &\necho '> Task :apps:lobby:compileJava'\necho 'Lobby.java:3: error: missing' >&2\nexit 23\n");
    let project = fixture.project();
    let error = execute(&project, BuildMode::Release, CancellationToken::new(), Progress::default())
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("Gradle chunkArtifacts failed"));
    assert!(error.contains("23"));
    assert!(error.contains("Lobby.java:3: error: missing"));
    assert!(!error.contains("> Task"));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!fixture.root.join("failed-leak").exists());
    fixture.wrapper("exit 0\n");
    let error =
        execute(&project, BuildMode::Release, CancellationToken::new(), Progress::default()).await.err().unwrap();
    assert!(error.to_string().contains("Gradle JVM descriptor"));
    assert!(error.to_string().contains(".chunk/build/jvm/artifacts.json"));
    fs::remove_file(fixture.root.join("gradlew")).unwrap();
    let error =
        execute(&project, BuildMode::Release, CancellationToken::new(), Progress::default()).await.err().unwrap();
    assert!(error.to_string().contains("project Gradle wrapper missing"));
    assert!(!project.output.exists());
}

#[tokio::test]
async fn build_output_streams_before_exit_and_drains_the_final_line() {
    let fixture = Fixture::new();
    fixture.wrapper("echo 'stdout started'\necho 'stderr started' >&2\nwhile [ ! -f continue ]; do sleep 0.01; done\nprintf 'final line'\n");
    let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
    let progress = Progress::new(move |event| {
        let _ = sender.send(event);
    });
    let root = fixture.root.clone();
    let running = tokio::spawn(async move {
        gradle::run(&root, &std::env::current_exe().unwrap(), BuildMode::Dev, &CancellationToken::new(), &progress)
            .await
    });
    let mut lines = Vec::new();
    for _ in 0..2 {
        let event = tokio::time::timeout(Duration::from_secs(3), events.recv()).await.unwrap().unwrap();
        let Event::Output(line) = event else { panic!("expected build output") };
        lines.push(line);
    }
    lines.sort();
    assert_eq!(lines, ["stderr started", "stdout started"]);
    assert!(!running.is_finished());
    fs::write(fixture.root.join("continue"), "").unwrap();
    tokio::time::timeout(Duration::from_secs(3), running).await.unwrap().unwrap().unwrap();
    let Event::Output(line) = events.recv().await.unwrap() else { panic!("expected final output") };
    assert_eq!(line, "final line");
}

#[tokio::test]
async fn cancellation_and_dropped_builds_stop_wrapper_descendants() {
    for cancel in [true, false] {
        let fixture = Fixture::new();
        fixture.wrapper("(sleep 0.3; printf leaked > leaked) &\nprintf started > started\nwait\n");
        let stop = CancellationToken::new();
        let executable = std::env::current_exe().unwrap();
        let progress = Progress::default();
        let mut running = Box::pin(gradle::run(&fixture.root, &executable, BuildMode::Dev, &stop, &progress));
        tokio::select! {
            result = &mut running => panic!("wrapper exited before cancellation: {result:?}"),
            () = async {
                tokio::time::timeout(Duration::from_secs(3), async {
                    while !fixture.root.join("started").exists() { tokio::time::sleep(Duration::from_millis(5)).await; }
                }).await.unwrap();
            } => {}
        }
        if cancel {
            stop.cancel();
            assert_eq!(running.as_mut().await.unwrap_err().kind(), io::ErrorKind::Interrupted);
        }
        drop(running);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!fixture.root.join("leaked").exists());
    }
}

#[test]
fn build_failures_keep_diagnostics_and_drop_gradle_progress() {
    let log = "\
Starting a Gradle Daemon (subsequent builds will be faster)
> Configure project :apps:lobby
> Task :apps:lobby:chunkGenerate UP-TO-DATE
●  Generating backend clients…
│
◆  Generated → /project/.chunk/generated/jvm
> Task :apps:lobby:compileKotlin FAILED
w: Lobby.kt:1:1 Deprecated API
e: file:///project/apps/lobby/src/Lobby.kt:10:5 Unresolved reference 'foo'.


[Incubating] Problems report is available at: file:///project/build/reports/problems/problems-report.html

FAILURE: Build failed with an exception.

* What went wrong:
Execution failed for task ':apps:lobby:compileKotlin'.
> Compilation error. See log for more details

* Try:
> Run with --stacktrace option to get the stack trace.
> Get more help at https://help.gradle.org.

BUILD FAILED in 3s
4 actionable tasks: 1 executed, 3 up-to-date
";
    assert_eq!(
        output::excerpt(log.lines()),
        "\
> Task :apps:lobby:compileKotlin FAILED
e: file:///project/apps/lobby/src/Lobby.kt:10:5 Unresolved reference 'foo'.

FAILURE: Build failed with an exception.

* What went wrong:
Execution failed for task ':apps:lobby:compileKotlin'.
> Compilation error. See log for more details

BUILD FAILED in 3s"
    );
}
