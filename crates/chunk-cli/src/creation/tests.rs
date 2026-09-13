use super::*;
use std::path::Path;

fn source(root: &Path) -> PathBuf {
    let source = root.join("source checkout");
    for name in WRAPPER.iter().map(|(name, _)| *name).chain([
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
        format!("[versions]\nchunk = {:?}\nkotlin = \"9.8.7\"\nfoojay = \"6.5.4\"\n", env!("CARGO_PKG_VERSION")),
    )
    .unwrap();
    source
}

#[test]
fn cli_creates_both_languages_without_adjacent_sdk_files() {
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("chunk");
    let catalog: toml::Table = toml::from_str(include_str!("../../../../gradle/libs.versions.toml")).unwrap();
    let versions = &catalog["versions"];
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
        assert!(settings.contains(&format!("version {:?}", versions["foojay"].as_str().unwrap())));
        assert_eq!(
            settings.contains(&format!("version {:?}", versions["kotlin"].as_str().unwrap())),
            matches!(language, Language::Kotlin),
        );
        assert!(settings.contains(&format!("rootProject.name = {name:?}")));
        let properties = fs::read_to_string(options.directory.join("gradle.properties")).unwrap();
        assert!(!properties.contains("chunk.source="));
        assert!(!properties.contains("sdk/maven"));
        for &(name, contents) in WRAPPER {
            assert_eq!(fs::read(options.directory.join(name)).unwrap(), contents);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(fs::metadata(options.directory.join("gradlew")).unwrap().permissions().mode() & 0o111, 0);
        }
        let readme = fs::read_to_string(options.directory.join("README.md")).unwrap();
        assert!(readme.starts_with(&format!("# {name}\n")));
        assert!(readme.contains(&format!("{} dev", shell(&executable).unwrap())));
    }
}

#[test]
fn source_override_has_discoverable_apps_and_refuses_nonempty_directories() {
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
        let settings = fs::read_to_string(options.directory.join("settings.gradle.kts")).unwrap();
        assert!(settings.contains("version \"6.5.4\""));
        assert_eq!(settings.contains("version \"9.8.7\""), matches!(language, Language::Kotlin));
        for &(name, _) in WRAPPER {
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
fn mismatched_checkout_leaves_the_destination_untouched() {
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("chunk");
    let options = Options { directory: root.path().join("project"), language: Language::Java, chunk_source: None };
    let source = source(root.path());
    fs::write(
        source.join("gradle/libs.versions.toml"),
        "[versions]\nchunk = \"999.0.0\"\nkotlin = \"2.4.10\"\nfoojay = \"1.0.0\"\n",
    )
    .unwrap();
    let options = Options { chunk_source: Some(source), ..options };
    assert_eq!(
        create(&options, &executable).unwrap_err().to_string(),
        format!("CLI and checkout versions must match: CLI {}, checkout 999.0.0", env!("CARGO_PKG_VERSION")),
    );
    assert!(!options.directory.exists());
}

#[test]
fn invalid_destinations_report_missing_parents_and_symlinks_without_creating_files() {
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("chunk");
    let parent = root.path().join("missing/parent");
    let options = Options { directory: parent.join("project"), language: Language::Java, chunk_source: None };
    let error = create(&options, &executable).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert_eq!(error.to_string(), format!("project parent directory does not exist: {}", parent.display()));
    assert!(!root.path().join("missing").exists());

    #[cfg(unix)]
    {
        let empty = root.path().join("empty");
        fs::create_dir(&empty).unwrap();
        let link = root.path().join("linked");
        std::os::unix::fs::symlink(&empty, &link).unwrap();
        let options = Options { directory: link.clone(), ..options };
        assert_eq!(
            create(&options, &executable).unwrap_err().to_string(),
            format!("project directory must not be a symlink: {}", link.display()),
        );
        assert_eq!(fs::read_link(link).unwrap(), empty);
        assert!(fs::read_dir(empty).unwrap().next().is_none());
    }
}

#[cfg(unix)]
#[test]
fn existing_project_needs_no_write_access_to_its_parent() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("chunk");
    let parent = root.path().join("readonly");
    let project = parent.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::set_permissions(&project, fs::Permissions::from_mode(0o755)).unwrap();
    let permissions = fs::metadata(&parent).unwrap().permissions();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
    let options = Options { directory: project.join("."), language: Language::Kotlin, chunk_source: None };
    let result = create(&options, &executable);
    fs::set_permissions(&parent, permissions).unwrap();
    result.unwrap();
    assert!(project.join("settings.gradle.kts").is_file());
}
