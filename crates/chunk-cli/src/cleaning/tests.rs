use super::*;

fn write(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, b"generated").unwrap();
}

#[test]
fn clean_removes_generated_state_and_keeps_backend_data_unless_asked() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let generated = [
        "dist/release/release.json",
        ".chunk/build/jvm/artifacts.json",
        ".chunk/sdk/index.ts",
        ".chunk/local/releases/release/release.json",
        ".chunk/local/control/deployment/directory.sqlite",
        ".chunk/local/deployment.json",
        ".chunk/custom/releases/release/release.json",
    ];
    let backends = [".chunk/local/backend/environment.sqlite", ".chunk/custom/backend/environment.sqlite"];
    for path in generated.iter().chain(&backends).chain(&[".chunk/local/runner.lock", ".chunk/custom/runner.lock"]) {
        write(&root.join(path));
    }
    let options = |data| Options { project: root.into(), data };
    assert!(clean(&options(false)).err().unwrap().to_string().contains("chunk.toml"));
    fs::write(root.join("chunk.toml"), "").unwrap();

    // Concurrent tests' child processes can briefly inherit a released lock, so only refusal takes it here.
    let _running = crate::local::runner_lock(&root.join(PROJECT_LOCK)).unwrap();
    assert!(clean(&options(false)).err().unwrap().to_string().contains("stop chunk dev"));
    assert!(root.join("dist").exists());

    let Cleaned { mut removed, kept } = remove_generated(root, false).unwrap();
    removed.sort();
    assert_eq!(
        removed,
        [
            ".chunk/build",
            ".chunk/custom/releases",
            ".chunk/local/control",
            ".chunk/local/deployment.json",
            ".chunk/local/releases",
            ".chunk/sdk",
            "dist"
        ]
    );
    assert_eq!(kept.len(), 2);
    assert!(generated.iter().all(|path| !root.join(path).exists()));
    assert!(backends.iter().all(|path| root.join(path).exists()));
    let mut removed = remove_generated(root, true).unwrap().removed;
    removed.sort();
    assert_eq!(removed, [".chunk/custom/backend", ".chunk/local/backend"]);
    assert!(root.join(PROJECT_LOCK).exists());
}

#[cfg(unix)]
#[test]
fn clean_refuses_to_prune_through_a_symlink() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("project");
    write(&root.join("chunk.toml"));
    write(&directory.path().join("unrelated/important.txt"));
    std::os::unix::fs::symlink("../unrelated", root.join(".chunk")).unwrap();

    let options = Options { project: root, data: true };
    assert!(clean(&options).err().unwrap().to_string().contains("symlink"));
    assert!(directory.path().join("unrelated/important.txt").exists());
}
