use super::*;

#[test]
fn local_control_uses_discovered_apps_and_resolved_runtime_requirements() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("chunk.toml"), "[local]\nenvironment='development'\nmachine_profile='small'\ncapacity=16\nmax_processes=4\n[local.profiles.small]\nmemory_mib=512\nmax_sessions=2\n[local.profiles.large]\nmemory_mib=1024\nmax_sessions=4\n").unwrap();
    for (id, manifest) in [("lobby", ""), ("arena", "[runtime]\nmachine_profile='large'\ncapacity=8\n")] {
        let directory = root.path().join("apps").join(id);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("app.toml"), manifest).unwrap();
        fs::write(directory.join("build.gradle.kts"), "").unwrap();
    }
    let metadata = chunk_build::project::inspect(root.path()).unwrap();
    let config =
        control_config(&metadata, "release-id", &[app("arena", "large", 8), app("lobby", "small", 16)]).unwrap();
    assert_eq!(config.deployment.environment, "development");
    assert_eq!(config.deployment.deployment, "release-id");
    assert_eq!(config.artifact_digest, "release-id");
    assert_eq!(config.session_types.keys().map(String::as_str).collect::<Vec<_>>(), ["arena/default", "lobby/default"]);
    assert_eq!(config.session_types["arena/default"].machine_profile, "large");
    assert_eq!(config.session_types["arena/default"].capacity, 8);
    assert_eq!(config.session_types["lobby/default"].machine_profile, "small");
    assert_eq!(config.session_types["lobby/default"].capacity, 16);
    assert_eq!(config.profiles["large"].memory_mib, 1024);
    assert_eq!(config.profiles["small"].max_sessions, 2);
    assert_eq!(config.max_processes, 4);
    fs::write(root.path().join("chunk.toml"), "").unwrap();
    fs::write(root.path().join("apps/arena/app.toml"), "").unwrap();
    assert!(
        control_config(&chunk_build::project::inspect(root.path()).unwrap(), "release-id", &[])
            .err()
            .unwrap()
            .to_string()
            .contains("[local]")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn java_executable_must_meet_the_descriptor_requirement() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let java = root.path().join("java");
    fs::write(&java, "#!/bin/sh\nprintf 'openjdk version \"26-ea\"\\n' >&2\n").unwrap();
    fs::set_permissions(&java, fs::Permissions::from_mode(0o755)).unwrap();
    java_version(&java, 26).await.unwrap();
    let error = java_version(&java, 27).await.unwrap_err();
    assert!(error.to_string().contains("Java 27+"));
    assert!(error.to_string().contains(java.to_str().unwrap()));
}

pub(super) fn app(id: &str, profile: &str, capacity: u32) -> chunk_contract::AppArtifact {
    serde_json::from_value(serde_json::json!({"id":id,"jar":"app.jar","sha256":"artifact","java_version":25,
        "sessions":{"default":{"machine_profile":profile,"capacity":capacity}}}))
    .unwrap()
}
