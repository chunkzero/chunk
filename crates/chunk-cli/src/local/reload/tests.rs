use super::*;
use chunk_proto::v1::ProcessHealth;

fn release(id: &str, jar: &str) -> Release {
    Release {
        id: id.into(),
        directory: PathBuf::new(),
        archive: PathBuf::new(),
        apps: vec![super::super::tests::app("lobby", "small", 16)]
            .into_iter()
            .map(|app| chunk_contract::AppArtifact { sha256: jar.into(), ..app })
            .collect(),
    }
}

fn node(phase: NodePhase, players: u32) -> NodeStatus {
    NodeStatus {
        phase: phase.into(),
        health: Some(ProcessHealth { players, ..Default::default() }),
        ..Default::default()
    }
}

#[test]
fn identical_jars_make_a_backend_only_reload() {
    let current = release("a", "jar-1");
    assert_eq!(classify(&current, &release("a", "jar-1")), Change::Unchanged);
    assert_eq!(classify(&current, &release("b", "jar-1")), Change::Backend);
    assert_eq!(classify(&current, &release("c", "jar-2")), Change::Jvm);
}

#[test]
fn pinned_releases_stop_only_after_staying_empty() {
    let start = Instant::now();
    let mut pinned = Retirement::pinned();
    assert!(!pinned.due(Some(&[node(NodePhase::Online, 2)]), start));
    assert!(!pinned.due(None, start + Duration::from_secs(3600)));
    assert!(!pinned.due(Some(&[node(NodePhase::Online, 0)]), start + Duration::from_secs(1)));
    assert!(!pinned.due(Some(&[node(NodePhase::Starting, 0)]), start + Duration::from_secs(5)));
    assert!(!pinned.due(Some(&[node(NodePhase::Online, 0)]), start + Duration::from_secs(6)));
    assert!(pinned.due(Some(&[node(NodePhase::Online, 0)]), start + Duration::from_secs(16)));
    assert!(Retirement::pinned().due(Some(&[node(NodePhase::Stopped, 0)]), start));
    assert!(Retirement::pinned().due(Some(&[]), start));
}

#[test]
fn draining_releases_stop_at_the_deadline_with_players_remaining() {
    let start = Instant::now();
    let mut draining = Retirement::until(start + Duration::from_secs(30));
    assert!(!draining.due(Some(&[node(NodePhase::Online, 3)]), start + Duration::from_secs(29)));
    assert!(draining.due(None, start + Duration::from_secs(30)));
    let mut pinned = Retirement::pinned();
    pinned.drain_by(start + Duration::from_secs(5));
    pinned.drain_by(start + Duration::from_secs(60));
    assert!(pinned.due(Some(&[node(NodePhase::Online, 3)]), start + Duration::from_secs(5)));
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
