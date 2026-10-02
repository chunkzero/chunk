use super::*;
use std::time::Duration;

fn release(id: &str, jar: &str) -> Release {
    Release {
        id: id.into(),
        directory: PathBuf::new(),
        archive: None,
        apps: vec![super::super::tests::app("lobby", "small", 16)]
            .into_iter()
            .map(|app| chunk_contract::AppArtifact { sha256: jar.into(), ..app })
            .collect(),
    }
}

#[test]
fn identical_jars_make_a_backend_only_reload() {
    let current = release("a", "jar-1");
    assert_eq!(classify((&current, "assets"), (&release("a", "jar-1"), "assets")), Change::Unchanged);
    assert_eq!(classify((&current, "assets"), (&release("b", "jar-1"), "assets")), Change::Backend);
    assert_eq!(classify((&current, "assets"), (&release("c", "jar-2"), "assets")), Change::Jvm);
    assert_eq!(classify((&current, "assets"), (&release("a", "jar-1"), "changed")), Change::Jvm);
}

#[test]
fn only_project_sources_trigger_rebuilds() {
    let root = Path::new("/project");
    let ignored = [PathBuf::from("/project/out/releases")];
    for path in ["apps/lobby/src/Lobby.kt", "apps/lobby/app.ts", "chunk.toml", "shared/build.gradle.kts"] {
        assert!(relevant(root, &ignored, &root.join(path)), "{path}");
    }
    for path in [
        "apps/lobby/build/libs/lobby.jar",
        ".chunk/generated/index.ts",
        ".gradle/caches/file",
        "dist/release.json",
        "node_modules/x/index.js",
        "out/releases/id/release.json",
        "apps/lobby/src/.Lobby.kt.swp",
        "apps/lobby/src/Lobby.kt~",
        "",
    ] {
        assert!(!relevant(root, &ignored, &root.join(path)), "{path}");
    }
    assert!(!relevant(root, &ignored, Path::new("/elsewhere/chunk.toml")));
}

#[test]
fn top_level_directories_created_after_startup_are_watched() {
    fn next(changes: &mut mpsc::UnboundedReceiver<PathBuf>) -> Option<PathBuf> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(path) = changes.try_recv() {
                return Some(path);
            }
            if std::time::Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let root = tempfile::tempdir().unwrap();
    let (mut watcher, mut changes) = watch(root.path(), &[]).unwrap();
    let assets = root.path().join("assets");
    std::fs::create_dir(&assets).unwrap();
    watcher.cover(&next(&mut changes).unwrap());
    while changes.try_recv().is_ok() {}
    std::fs::write(assets.join("example.txt"), "").unwrap();
    assert!(std::iter::from_fn(|| next(&mut changes)).any(|path| path == assets.join("example.txt")));
}

#[test]
fn a_drain_asks_control_for_the_same_deadline_however_long_it_has_run() {
    let mut drain = Drain::until(Some(Duration::from_secs(30)));
    drain.since = Instant::now().checked_sub(Duration::from_secs(20)).unwrap();
    assert_eq!(drain.policy().deadline, Some(Duration::from_secs(30)));
}

#[test]
fn superseding_a_pinned_release_counts_the_deadline_from_its_drain_start() {
    let mut pinned = Drain::until(None);
    pinned.since = Instant::now().checked_sub(Duration::from_secs(3600)).unwrap();
    pinned.drain_within(Duration::from_secs(30));
    let deadline = pinned.policy().deadline.unwrap();
    assert!(deadline >= Duration::from_secs(3630) && deadline < Duration::from_secs(3640));
    pinned.drain_within(Duration::from_secs(300));
    assert_eq!(pinned.policy().deadline, Some(deadline));
}
