use super::ensure_inside;
use super::plan::{Entry, Pull, Push, check_push, decide, entries, locate, summary};
use chunk_build::project::{AppMetadata, ProjectMetadata, RuntimeRequirements, World};
use chunk_contract::{AssetBlob, AssetRevision};
use std::collections::BTreeMap;

#[test]
fn a_pull_never_overwrites_a_modified_file() {
    let (a, b, c) = (Some("a"), Some("b"), Some("c"));
    // Head unchanged since the base: nothing to do, however the file changed.
    assert_eq!(decide(a, a, c), Pull::Keep);
    assert_eq!(decide(None, None, c), Pull::Keep);
    // The file is still the base: take the head, or its removal.
    assert_eq!(decide(a, b, a), Pull::Write);
    assert_eq!(decide(None, b, None), Pull::Write);
    assert_eq!(decide(a, None, a), Pull::Delete);
    // The file already has the head's content.
    assert_eq!(decide(a, b, b), Pull::Keep);
    assert_eq!(decide(a, None, None), Pull::Keep);
    // Both sides changed.
    assert_eq!(decide(a, b, c), Pull::Conflict);
    assert_eq!(decide(None, b, c), Pull::Conflict);
    assert_eq!(decide(a, None, c), Pull::Conflict);
}

#[test]
fn a_push_refuses_when_the_head_moved_unless_forced() {
    assert_eq!(check_push("", None, "built", false), Push::Send);
    assert_eq!(check_push("head", Some("head"), "built", false), Push::Send);
    assert_eq!(check_push("head", Some("older"), "built", false), Push::Moved);
    assert_eq!(check_push("head", None, "built", false), Push::Moved);
    assert_eq!(check_push("head", Some("older"), "built", true), Push::Send);
    assert_eq!(check_push("head", Some("older"), "head", false), Push::UpToDate);
}

#[test]
fn the_summary_counts_added_changed_and_removed_entries_by_kind() {
    let blob = |sha256: &str| AssetBlob { sha256: sha256.into(), size: 1 };
    let mut old = AssetRevision { version: 1, ..AssetRevision::default() };
    old.shared.insert("a.txt".into(), blob("1"));
    old.shared.insert("b.txt".into(), blob("2"));
    old.apps.entry("arena".into()).or_default().worlds.insert("koth".into(), blob("3"));
    let mut new = AssetRevision { version: 1, ..AssetRevision::default() };
    new.shared.insert("a.txt".into(), blob("9"));
    new.shared.insert("c.txt".into(), blob("4"));
    new.apps.entry("arena".into()).or_default().worlds.insert("koth".into(), blob("3"));
    new.apps.entry("arena".into()).or_default().worlds.insert("hub".into(), blob("5"));
    assert_eq!(summary(&entries(&old), &entries(&new)), ["files: 1 added, 1 changed, 1 removed", "worlds: 1 added"]);
    assert!(summary(&entries(&old), &entries(&old)).is_empty());
}

#[cfg(unix)]
#[test]
fn a_pull_never_writes_through_a_symlink() {
    let (root, elsewhere) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::create_dir(root.path().join("assets")).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), root.path().join("assets/config")).unwrap();
    ensure_inside(root.path(), &root.path().join("assets/plain/new.json")).unwrap();
    let error = ensure_inside(root.path(), &root.path().join("assets/config/new.json")).unwrap_err();
    assert!(error.to_string().contains("symlink"));
    assert!(ensure_inside(root.path(), &root.path().join("assets/../../x")).is_err());
}

#[test]
fn a_pull_skips_a_world_whose_source_is_not_a_file() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("apps/arena/assets/save.polar")).unwrap();
    let world = |source: &str| World { source: source.into() };
    let app = AppMetadata {
        id: "arena".into(),
        directory: "apps/arena".into(),
        gradle_project: ":arena".into(),
        domain: String::new(),
        runtime: RuntimeRequirements::default(),
        sessions: BTreeMap::default(),
        worlds: [
            ("save".into(), world("apps/arena/assets/save.polar")),
            ("lobby".into(), world("apps/arena/assets/lobby.polar")),
        ]
        .into(),
        packs: BTreeMap::default(),
    };
    let metadata = ProjectMetadata { version: 1, apps: vec![app], local: None, scope_packs: BTreeMap::default() };
    let locate = |entry: Entry| locate(&entry, root.path(), &metadata);
    assert_eq!(
        locate(Entry::World("arena".into(), "save".into())).unwrap_err(),
        "world source apps/arena/assets/save.polar is not a file"
    );
    assert_eq!(
        locate(Entry::World("arena".into(), "lobby".into())).unwrap(),
        root.path().join("apps/arena/assets/lobby.polar")
    );
    assert_eq!(locate(Entry::Shared("a.txt".into())).unwrap(), root.path().join("assets/a.txt"));
}
