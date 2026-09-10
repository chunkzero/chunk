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
        fs::create_dir(root.join("fixture")).unwrap();
        for name in ["chunk.toml", "apps/lobby/app.toml", "apps/lobby/build.gradle.kts"] {
            fs::write(root.join(name), "").unwrap();
        }
        let mut jar = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        for (name, bytes) in [
            ("META-INF/chunk/app.json", br#"{"version":1,"id":"lobby"}"#.as_slice()),
            (
                "META-INF/services/dev.chunkzero.runtime.SessionProvider",
                b"sample.Provider\n".as_slice(),
            ),
            (
                "sample/Provider.class",
                [0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 69].as_slice(),
            ),
        ] {
            jar.start_file(name, options).unwrap();
            jar.write_all(bytes).unwrap();
        }
        fs::write(root.join("fixture/lobby.jar"), jar.finish().unwrap().into_inner()).unwrap();
        fs::write(root.join("fixture/source.mjs"), "export const value = 1;").unwrap();
        fs::write(
            root.join("fixture/contract.json"),
            br#"{"contract_version":1,"runtime_profile":"transactional_v1","tables":{},"functions":{}}"#,
        )
        .unwrap();
        fs::write(
            root.join("fixture/artifacts.json"),
            serde_json::to_vec(&json!({
                "version":1,"java":{"version":25,"executable":root.join("jdk/bin/java")},
                "apps":[{"id":"lobby","jar":root.join("fixture/lobby.jar"),"java_version":25}],"classpath":[]
            }))
            .unwrap(),
        )
        .unwrap();
        let fixture = Self {
            _directory: directory,
            root,
        };
        fixture.wrapper("printf '%s\\n' \"$PWD\" \"$@\" > wrapper-arguments.txt\nmkdir -p .chunk/build/backend .chunk/build/jvm\ncp fixture/source.mjs fixture/contract.json .chunk/build/backend/\ncp fixture/artifacts.json .chunk/build/jvm/artifacts.json\n");
        fixture
    }

    fn wrapper(&self, source: &str) {
        let path = self.root.join("gradlew");
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{source}")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn project(&self) -> Project {
        prepare(&Options {
            project: self.root.clone(),
            output: None,
        })
        .unwrap()
    }
}

#[tokio::test]
async fn project_build_packages_only_after_the_requested_gradle_task_finishes() {
    let fixture = Fixture::new();
    let project = fixture.project();
    assert!(!project.output.exists());
    assert!(!fixture.root.join(".chunk/build/backend").exists());
    let built = execute(&project, CancellationToken::new()).await.unwrap();
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
    assert_eq!(built.release.archive.parent().unwrap(), fixture.root.join("dist"));
    assert!(built.release.archive.is_file());
    assert!(built.release.directory.join("backend.json").is_file());
    assert_eq!(built.java.version, 25);
    assert_eq!(built.java.executable, fixture.root.join("jdk/bin/java"));
    let explicit = prepare(&Options {
        project: fixture.root.clone(),
        output: Some("target/consumer-releases".into()),
    })
    .unwrap();
    assert_eq!(
        explicit.output,
        std::env::current_dir().unwrap().join("target/consumer-releases")
    );
    assert!(
        prepare(&Options {
            project: fixture.root.clone(),
            output: Some(fixture.root.join(".chunk/build"))
        })
        .is_err()
    );
}

#[tokio::test]
async fn invalid_metadata_wrapper_failure_and_missing_descriptors_do_not_publish() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("chunk.toml"), "domains=[]").unwrap();
    assert!(
        prepare(&Options {
            project: fixture.root.clone(),
            output: None
        })
        .err()
        .unwrap()
        .to_string()
        .contains("unknown field")
    );
    assert!(!fixture.root.join("wrapper-arguments.txt").exists());
    fs::write(fixture.root.join("chunk.toml"), "").unwrap();
    fixture.wrapper("(sleep 0.3; printf leaked > failed-leak) &\nexit 23\n");
    let project = fixture.project();
    let error = execute(&project, CancellationToken::new()).await.err().unwrap();
    assert!(error.to_string().contains("Gradle chunkArtifacts failed"));
    assert!(error.to_string().contains("23"));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!fixture.root.join("failed-leak").exists());
    fixture.wrapper("exit 0\n");
    let error = execute(&project, CancellationToken::new()).await.err().unwrap();
    assert!(error.to_string().contains("Gradle JVM descriptor"));
    assert!(error.to_string().contains(".chunk/build/jvm/artifacts.json"));
    fs::remove_file(fixture.root.join("gradlew")).unwrap();
    let error = execute(&project, CancellationToken::new()).await.err().unwrap();
    assert!(error.to_string().contains("project Gradle wrapper missing"));
    assert!(!project.output.exists());
}

#[tokio::test]
async fn cancellation_and_dropped_builds_stop_wrapper_descendants() {
    for cancel in [true, false] {
        let fixture = Fixture::new();
        fixture.wrapper("(sleep 0.3; printf leaked > leaked) &\nprintf started > started\nwait\n");
        let stop = CancellationToken::new();
        let executable = std::env::current_exe().unwrap();
        let mut running = Box::pin(gradle::run(&fixture.root, &executable, &stop));
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
