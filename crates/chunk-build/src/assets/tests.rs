use std::{fs, io::Read, path::Path};

use super::*;

const POLAR: &[u8] = b"Polr\0\x07world";

fn write(root: &Path, path: &str, bytes: &[u8]) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// A project whose arena app has two worlds and a pack, below a root scope with another pack.
fn project(root: &Path) {
    write(root, "chunk.toml", b"");
    write(root, "apps/scope.ts", b"export default defineScope({packs:{base:{source:'packs/base.zip'}}});");
    write(
        root,
        "apps/arena/app.ts",
        b"export default defineApp({id:'arena',worlds:{lobby:{source:'worlds/lobby.polar'},koth:{source:'worlds/koth.polar'}},packs:{ui:{source:'packs/ui',required:true}}});",
    );
    write(root, "apps/arena/build.gradle.kts", b"");
    write(root, "apps/arena/assets/worlds/lobby.polar", POLAR);
    write(root, "apps/arena/assets/worlds/koth.polar", b"Polr koth");
    write(root, "apps/arena/assets/packs/ui/pack.mcmeta", b"{}");
    write(root, "apps/arena/assets/packs/ui/assets/minecraft/lang/en_us.json", b"{}");
    write(root, "apps/arena/assets/kits/default.json", b"kit");
    write(root, "assets/config/game.json", b"config");
    let mut zip = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
    zip.start_file("pack.mcmeta", zip::write::SimpleFileOptions::default()).unwrap();
    write(root, "assets/packs/base.zip", &zip.finish().unwrap().into_inner());
}

#[test]
fn revisions_hold_plain_files_worlds_and_reproducible_packs() {
    let project_directory = tempfile::tempdir().unwrap();
    let root = project_directory.path();
    project(root);
    let first = Store::new(root.join("dist/assets"));

    let revision = build_revision(root, &first).unwrap();
    assert_eq!(revision.shared.keys().collect::<Vec<_>>(), ["config/game.json"]);
    let arena = &revision.apps["arena"];
    assert_eq!(arena.files.keys().collect::<Vec<_>>(), ["kits/default.json"]);
    assert_eq!(arena.worlds.keys().collect::<Vec<_>>(), ["koth", "lobby"]);
    assert_eq!(revision.packs.keys().collect::<Vec<_>>(), ["base", "ui"]);
    assert_eq!(first.read_revision(&revision.id()).unwrap(), revision);
    let ui = &revision.packs["ui"];
    let mut zip = zip::ZipArchive::new(fs::File::open(first.root().join("blobs").join(&ui.sha256)).unwrap()).unwrap();
    assert_eq!(zip.file_names().count(), 2);
    let mut mcmeta = String::new();
    zip.by_name("pack.mcmeta").unwrap().read_to_string(&mut mcmeta).unwrap();
    assert_eq!(mcmeta, "{}");

    // Touching sources changes nothing; another store gets the same revision.
    fs::write(root.join("apps/arena/assets/packs/ui/pack.mcmeta"), b"{}").unwrap();
    let second = build_revision(root, &Store::new(root.join("other"))).unwrap();
    assert_eq!(second.id(), revision.id());

    write(root, "apps/arena/assets/worlds/koth.polar", b"not polar");
    let error = build_revision(root, &first).unwrap_err().to_string();
    assert!(error.contains("not a Polar world"), "{error}");
}

#[test]
fn materialized_directories_link_read_only_blobs_and_are_reused() {
    let project_directory = tempfile::tempdir().unwrap();
    let root = project_directory.path();
    project(root);
    let store = Store::new(root.join("store"));
    let revision = build_revision(root, &store).unwrap();

    let directory = materialize(&store, &revision, "arena").unwrap();
    assert_eq!(directory, store.root().join("apps").join(revision.id()).join("arena"));
    assert_eq!(fs::read(directory.join("revision.json")).unwrap(), revision.encode());
    assert_eq!(fs::read(directory.join("worlds/lobby.polar")).unwrap(), POLAR);
    assert_eq!(fs::read(directory.join("worlds/koth.polar")).unwrap(), b"Polr koth");
    assert_eq!(fs::read(directory.join("app/kits/default.json")).unwrap(), b"kit");
    assert_eq!(fs::read(directory.join("shared/config/game.json")).unwrap(), b"config");
    assert!(!directory.join("packs").exists() && !directory.join("app/worlds").exists());
    assert!(fs::metadata(directory.join("app/kits/default.json")).unwrap().permissions().readonly());
    assert_eq!(materialize(&store, &revision, "arena").unwrap(), directory);

    let lobby = materialize(&store, &revision, "lobby").unwrap();
    assert_eq!(fs::read_dir(lobby.join("worlds")).unwrap().count(), 0);
    assert_eq!(fs::read(lobby.join("shared/config/game.json")).unwrap(), b"config");

    let error = store.insert(&"0".repeat(64), b"other").unwrap_err();
    assert!(error.to_string().contains("asset blob"), "{error}");
    assert!(store.insert("../escape", b"").is_err());
}

#[test]
fn incomplete_materialized_directories_are_rebuilt() {
    let project_directory = tempfile::tempdir().unwrap();
    let root = project_directory.path();
    project(root);
    let store = Store::new(root.join("store"));
    let revision = build_revision(root, &store).unwrap();
    let directory = materialize(&store, &revision, "arena").unwrap();

    fs::remove_file(directory.join("worlds/lobby.polar")).unwrap();
    assert_eq!(materialize(&store, &revision, "arena").unwrap(), directory);
    assert_eq!(fs::read(directory.join("worlds/lobby.polar")).unwrap(), POLAR);

    fs::remove_file(directory.join("revision.json")).unwrap();
    materialize(&store, &revision, "arena").unwrap();
    assert_eq!(fs::read(directory.join("revision.json")).unwrap(), revision.encode());

    // Concurrent callers repair it once and all see the complete directory.
    fs::remove_file(directory.join("worlds/lobby.polar")).unwrap();
    std::thread::scope(|scope| {
        let callers: Vec<_> = (0..8).map(|_| scope.spawn(|| materialize(&store, &revision, "arena"))).collect();
        for caller in callers {
            assert_eq!(caller.join().unwrap().unwrap(), directory);
        }
    });
    assert_eq!(fs::read(directory.join("worlds/lobby.polar")).unwrap(), POLAR);
}
