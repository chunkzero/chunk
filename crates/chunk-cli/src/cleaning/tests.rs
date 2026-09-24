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
    ];
    for path in generated.iter().chain(&[".chunk/local/backend/environment.sqlite"]) {
        write(&root.join(path));
    }
    let options = |data| Options { project: root.into(), data };
    assert!(clean(&options(false)).unwrap_err().to_string().contains("chunk.toml"));
    fs::write(root.join("chunk.toml"), "").unwrap();

    // Concurrent tests' child processes can briefly inherit a released lock, so only refusal takes it here.
    let _running = crate::local::runner_lock(&root.join(".chunk/local/runner.lock")).unwrap();
    assert!(clean(&options(false)).unwrap_err().to_string().contains("stop chunk dev"));
    assert!(root.join("dist").exists());

    let mut removed = remove_generated(root, false).unwrap();
    removed.sort();
    assert_eq!(
        removed,
        [
            ".chunk/build",
            ".chunk/local/control",
            ".chunk/local/deployment.json",
            ".chunk/local/releases",
            ".chunk/sdk",
            "dist"
        ]
    );
    assert!(generated.iter().all(|path| !root.join(path).exists()));
    assert!(root.join(".chunk/local/backend/environment.sqlite").exists());
    assert_eq!(remove_generated(root, true).unwrap(), [".chunk/local/backend"]);
    assert!(remove_generated(root, true).unwrap().is_empty());
}
