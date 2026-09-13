use super::*;
use std::path::Path;

fn source(root: &Path) -> PathBuf {
    let source = root.join("source checkout");
    for name in WRAPPER.iter().copied().chain([
        "settings.gradle.kts",
        "jvm/gradle-plugin/settings.gradle.kts",
        "jvm/runtime-minestom/build.gradle.kts",
    ]) {
        let path = source.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, name).unwrap();
    }
    fs::write(
        source.join("gradle/libs.versions.toml"),
        format!("[versions]\nchunk = {:?}\nkotlin = \"2.4.10\"\nfoojay = \"1.0.0\"\n", env!("CARGO_PKG_VERSION")),
    )
    .unwrap();
    source
}

fn sdk(root: &Path) -> PathBuf {
    let sdk = root.join("installed sdk");
    fs::create_dir(&sdk).unwrap();
    fs::write(
        sdk.join("sdk.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema": 1,
            "version": env!("CARGO_PKG_VERSION"),
            "kotlin_version": "9.8.7",
            "foojay_version": "6.5.4",
            "maven_repository": "https://maven.chunkzero.com",
        }))
        .unwrap(),
    )
    .unwrap();
    for name in WRAPPER {
        let path = sdk.join("sdk/wrapper").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, name).unwrap();
    }
    sdk.join("chunk")
}

#[test]
fn installed_sdk_creates_both_languages_with_pinned_versions_and_remote_libraries() {
    let root = tempfile::tempdir().unwrap();
    let executable = sdk(root.path());
    for (language, name) in [(Language::Java, "new java server"), (Language::Kotlin, "new kotlin server")] {
        let options = Options { directory: root.path().join(name), language, chunk_source: None };
        if matches!(language, Language::Kotlin) {
            fs::create_dir(&options.directory).unwrap();
        }
        create(&options, &executable).unwrap();
        let metadata = chunk_build::project::inspect(&options.directory).unwrap();
        assert_eq!(metadata.apps.len(), 1);
        assert_eq!(metadata.apps[0].id, "lobby");
        let settings = fs::read_to_string(options.directory.join("settings.gradle.kts")).unwrap();
        assert!(settings.contains("https://maven.chunkzero.com"));
        assert!(settings.contains(&format!("version {:?}", env!("CARGO_PKG_VERSION"))));
        assert!(settings.contains("version \"6.5.4\""));
        assert_eq!(settings.contains("version \"9.8.7\""), matches!(language, Language::Kotlin));
        assert!(settings.contains(&format!("rootProject.name = {name:?}")));
        let properties = fs::read_to_string(options.directory.join("gradle.properties")).unwrap();
        assert!(!properties.contains("chunk.source="));
        assert!(!properties.contains("sdk/maven"));
        for name in WRAPPER {
            assert_eq!(fs::read(options.directory.join(name)).unwrap(), name.as_bytes());
        }
        let readme = fs::read_to_string(options.directory.join("README.md")).unwrap();
        assert!(readme.contains(&format!("{} dev", shell(&executable).unwrap())));
    }
}

#[test]
fn source_override_has_discoverable_apps_and_preserves_existing_files() {
    let root = tempfile::tempdir().unwrap();
    let source = source(root.path());
    for (language, name) in [(Language::Java, "java"), (Language::Kotlin, "kotlin")] {
        let options = Options { directory: root.path().join(name), language, chunk_source: Some(source.clone()) };
        create(&options, Path::new("/prepared tools/chunk")).unwrap();
        let metadata = chunk_build::project::inspect(&options.directory).unwrap();
        assert_eq!(metadata.apps.len(), 1);
        assert_eq!(metadata.apps[0].id, "lobby");
        assert_eq!(metadata.apps[0].runtime.machine_profile.as_deref(), Some("local"));
        assert!(metadata.local.is_some());
        assert!(!options.directory.join(".chunk").exists());
        for name in WRAPPER {
            assert_eq!(fs::read(options.directory.join(name)).unwrap(), fs::read(source.join(name)).unwrap());
        }
        fs::write(options.directory.join("server/greetings.ts"), "user code").unwrap();
        assert_eq!(create(&options, Path::new("chunk")).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(options.directory.join("server/greetings.ts")).unwrap(), "user code");
    }
    let empty = root.path().join("empty");
    fs::create_dir(&empty).unwrap();
    let options = Options { directory: empty, language: Language::Java, chunk_source: Some(source) };
    create(&options, Path::new("chunk")).unwrap();
}

#[test]
fn invalid_toolchain_leaves_no_project_and_paths_are_properties_not_code() {
    let root = tempfile::tempdir().unwrap();
    let source = source(root.path());
    fs::remove_file(source.join("gradle/wrapper/gradle-wrapper.jar")).unwrap();
    let options =
        Options { directory: root.path().join("project"), language: Language::Java, chunk_source: Some(source) };
    assert!(create(&options, Path::new("chunk")).unwrap_err().to_string().contains("gradle-wrapper.jar"));
    assert!(!options.directory.exists());
    assert_eq!(
        property(Path::new("C:\\tools here\\é😀\nchunk")).unwrap(),
        "C\\:\\\\tools\\ here\\\\\\u00e9\\ud83d\\ude00\\u000achunk"
    );
    assert_eq!(kotlin("a\"$b\n"), "\"a\\\"\\$b\\n\"");
    assert_eq!(shell(Path::new("/tools/it's chunk")).unwrap(), "'/tools/it'\"'\"'s chunk'");
}

#[test]
fn absent_or_mismatched_sdk_metadata_leaves_the_destination_untouched() {
    let root = tempfile::tempdir().unwrap();
    let executable = sdk(root.path());
    let options = Options { directory: root.path().join("project"), language: Language::Java, chunk_source: None };
    let metadata = executable.parent().unwrap().join("sdk.json");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&metadata).unwrap()).unwrap();
    value["version"] = "999.0.0".into();
    fs::write(&metadata, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(create(&options, &executable).unwrap_err().to_string().contains("versions must match"));
    fs::remove_file(metadata).unwrap();
    assert!(create(&options, &executable).unwrap_err().to_string().contains("Install a Chunk SDK"));
    assert!(!options.directory.exists());
}
