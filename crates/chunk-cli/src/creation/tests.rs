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
    source
}

#[test]
fn starters_have_discoverable_apps_and_preserve_existing_directories() {
    let root = tempfile::tempdir().unwrap();
    let source = source(root.path());
    for (language, name) in [(Language::Java, "java"), (Language::Kotlin, "kotlin")] {
        let options = Options { directory: root.path().join(name), language, chunk_source: source.clone() };
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
    let options = Options { directory: empty.clone(), language: Language::Java, chunk_source: source };
    assert_eq!(create(&options, Path::new("chunk")).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read_dir(empty).unwrap().count(), 0);
}

#[test]
fn invalid_toolchain_leaves_no_project_and_paths_are_properties_not_code() {
    let root = tempfile::tempdir().unwrap();
    let source = source(root.path());
    fs::remove_file(source.join("gradle/wrapper/gradle-wrapper.jar")).unwrap();
    let options = Options { directory: root.path().join("project"), language: Language::Java, chunk_source: source };
    assert!(create(&options, Path::new("chunk")).unwrap_err().to_string().contains("gradle-wrapper.jar"));
    assert!(!options.directory.exists());
    assert_eq!(
        property(Path::new("C:\\tools here\\é😀\nchunk")).unwrap(),
        "C\\:\\\\tools\\ here\\\\\\u00e9\\ud83d\\ude00\\u000achunk"
    );
}
