use super::*;

#[test]
fn publication_is_reproducible_and_new_inputs_cannot_change_a_running_artifact() {
    let root = tempfile::tempdir().unwrap();
    let distribution = root.path().join("distribution");
    fs::create_dir_all(distribution.join("lib")).unwrap();
    fs::write(distribution.join("lib/gameplay.jar"), b"gameplay-one").unwrap();
    let source = root.path().join("backend.mjs");
    fs::write(&source, "export function status() { return 1; }").unwrap();
    let contract = root.path().join("contract.json");
    fs::write(&contract, br#"{"tables":{},"functions":{"status":{"kind":"query","export":"status","arguments":{"type":"null"},"result":{"type":"integer"}}}}"#).unwrap();
    let inputs = Inputs {
        source,
        contract,
        distribution,
    };
    let artifacts = root.path().join("artifacts");
    let first = publish(&inputs, &artifacts, b"project").unwrap();
    let repeated = publish(&inputs, &artifacts, b"project").unwrap();
    assert_eq!(first.id, repeated.id);
    let bundle: Deployment = serde_json::from_slice(&fs::read(first.directory.join("backend.json")).unwrap()).unwrap();
    assert_eq!(bundle.id, first.id);
    fs::write(inputs.distribution.join("lib/gameplay.jar"), b"gameplay-two").unwrap();
    let second = publish(&inputs, &artifacts, b"project").unwrap();
    assert_ne!(first.id, second.id);
    assert_eq!(
        fs::read(first.directory.join("gameplay/lib/gameplay.jar")).unwrap(),
        b"gameplay-one"
    );
    assert_ne!(second.id, publish(&inputs, &artifacts, b"changed-project").unwrap().id);
    // Extra classpath entries invalidate a published artifact, even when its expected files remain intact.
    fs::write(second.directory.join("gameplay/lib/extra.jar"), b"unexpected").unwrap();
    assert!(publish(&inputs, &artifacts, b"project").is_err());
}

#[cfg(unix)]
#[test]
fn child_executable_remains_runnable_after_the_original_is_replaced() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let original = root.path().join("program");
    fs::write(&original, "#!/bin/sh\nexit 7\n").unwrap();
    fs::set_permissions(&original, fs::Permissions::from_mode(0o755)).unwrap();
    let pinned = pin_program(&original, &root.path().join("platform")).unwrap();
    fs::rename(&original, root.path().join("old-program")).unwrap();
    fs::write(&original, "#!/bin/sh\nexit 8\n").unwrap();
    fs::set_permissions(&original, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(std::process::Command::new(&pinned).status().unwrap().code(), Some(7));
    let next = pin_program(&original, &root.path().join("platform")).unwrap();
    assert_ne!(pinned, next);
    assert_eq!(std::process::Command::new(next).status().unwrap().code(), Some(8));
}
