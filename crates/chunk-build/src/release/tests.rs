use super::*;
use std::{
    fs,
    io::{Cursor, Read, Write},
};

use serde_json::{Value, json};
use zip::{ZipWriter, write::SimpleFileOptions};

struct Fixture {
    root: tempfile::TempDir,
    inputs: ReleaseInputs,
    descriptor: Value,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let backend = root.path().join("backend");
        fs::create_dir_all(&backend).unwrap();
        fs::create_dir_all(project.join("assets")).unwrap();
        fs::write(project.join("chunk.toml"), "[local]\nenvironment='private-environment'\nmachine_profile='small'\ncapacity=16\nmax_processes=4\n[local.profiles.small]\nmemory_mib=512\nmax_sessions=2\n").unwrap();
        fs::write(project.join("assets/terrain 世界.txt"), b"world template").unwrap();
        fs::write(project.join("assets").join(format!("{}.txt", "long".repeat(40))), b"long asset").unwrap();
        fs::write(project.join(".env"), b"PRIVATE_TOKEN=secret").unwrap();
        for app in ["arena", "lobby"] {
            fs::create_dir_all(project.join(format!("apps/{app}/assets"))).unwrap();
            fs::write(
                project.join(format!("apps/{app}/app.toml")),
                if app == "arena" { "[runtime]\ncapacity=8\n" } else { "" },
            )
            .unwrap();
            fs::write(project.join(format!("apps/{app}/build.gradle.kts")), "").unwrap();
            fs::write(project.join(format!("apps/{app}/assets/map.txt")), app).unwrap();
            write_app_jar(&root.path().join(format!("{app}.jar")), app, b"first");
        }
        write_jar(&root.path().join("library.jar"), &[("sample/Library.class", class(21, 1))]);
        write_jar(&root.path().join("generated.jar"), &[("generated/BackendTypes.class", class(21, 1))]);
        fs::write(backend.join("source.mjs"), "export function status() { return 1; }").unwrap();
        fs::write(backend.join("source.mjs.map"), "{}").unwrap();
        fs::write(backend.join("contract.json"), br#"{"contract_version":2,"runtime_profile":"transactional_v1","tables":{},"functions":{"status":{"kind":"query","visibility":"public","export":"status","arguments":{"type":"null"},"result":{"type":"integer"}}}}"#).unwrap();
        fs::create_dir_all(backend.join(".sdk")).unwrap();
        fs::write(backend.join(".sdk/cache"), b"build cache").unwrap();
        let descriptor = json!({
            "version":4, "java":{"version":25,"executable":root.path().join("jdk/bin/java")},
            "apps":[
                {"id":"lobby","jar":root.path().join("lobby.jar"),"classpath":[],"java_version":25,"sessions":["default"]},
                {"id":"arena","jar":root.path().join("arena.jar"),"classpath":[],"java_version":25,"sessions":["default"]}
            ]
        });
        let jvm_descriptor = root.path().join("artifacts.json");
        let fixture =
            Self { root, inputs: ReleaseInputs { project, backend, jvm_descriptor, archive: true }, descriptor };
        fixture.save_descriptor();
        fixture
    }

    fn save_descriptor(&self) {
        fs::write(&self.inputs.jvm_descriptor, serde_json::to_vec(&self.descriptor).unwrap()).unwrap();
    }

    fn publish(&self) -> io::Result<Release> {
        publish_release(&self.inputs, &self.root.path().join("dist"))
    }
}

fn class(java: u16, value: u8) -> Vec<u8> {
    let [high, low] = (java + 44).to_be_bytes();
    vec![0xca, 0xfe, 0xba, 0xbe, 0, 0, high, low, value]
}

fn write_jar(path: &Path, entries: &[(&str, Vec<u8>)]) {
    let mut jar = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in entries {
        jar.start_file(*name, options).unwrap();
        jar.write_all(bytes).unwrap();
    }
    fs::write(path, jar.finish().unwrap().into_inner()).unwrap();
}

fn write_app_jar(path: &Path, app: &str, marker: &[u8]) {
    write_jar(
        path,
        &[
            (
                "META-INF/MANIFEST.MF",
                format!("Manifest-Version: 1.0\r\nMain-Class: sample.{app}.Provider\r\n\r\n").into_bytes(),
            ),
            (&format!("sample/{app}/Provider.class"), class(25, 1)),
            ("marker.txt", marker.into()),
        ],
    );
}

#[test]
fn release_is_complete_and_reproducible_after_moving_all_local_inputs() {
    let first = Fixture::new();
    let mut moved = Fixture::new();
    moved.descriptor["apps"].as_array_mut().unwrap().reverse();
    moved.save_descriptor();
    let manifest = moved.inputs.project.join("chunk.toml");
    fs::write(
        &manifest,
        fs::read_to_string(&manifest)
            .unwrap()
            .replace("private-environment", "another-private-environment")
            .replace("max_processes=4", "max_processes=6"),
    )
    .unwrap();
    let a = first.publish().unwrap();
    let b = moved.publish().unwrap();
    assert_eq!(a.id, b.id);
    assert_eq!(a.id, first.publish().unwrap().id);
    let compressed = fs::read(a.archive.as_ref().unwrap()).unwrap();
    assert_eq!(compressed, fs::read(b.archive.unwrap()).unwrap());
    assert_eq!(&compressed[..8], &[0x1f, 0x8b, 8, 0, 0, 0, 0, 0]);
    assert_eq!(compressed[9], 255);
    assert_eq!(a.archive.as_ref().unwrap().parent(), a.directory.parent());
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(compressed.as_slice()));
    let mut archived = BTreeMap::new();
    let mut ordering = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let header = entry.header();
        assert!(header.entry_type().is_file());
        assert_eq!(
            (header.mode().unwrap(), header.uid().unwrap(), header.gid().unwrap(), header.mtime().unwrap()),
            (0o644, 0, 0, 0)
        );
        assert_eq!(header.username().unwrap(), Some(""));
        assert_eq!(header.groupname().unwrap(), Some(""));
        let name = entry.path().unwrap().to_str().unwrap().to_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, fs::read(a.directory.join(&name)).unwrap());
        ordering.push(name.clone());
        archived.insert(name, bytes);
    }
    assert_eq!(ordering, archived.keys().cloned().collect::<Vec<_>>());
    assert!(archived.contains_key("assets/terrain 世界.txt"));
    assert!(archived.contains_key(&format!("assets/{}.txt", "long".repeat(40))));
    assert!(archived.contains_key("apps/arena/assets/map.txt"));
    assert!(archived.contains_key("apps/lobby/assets/map.txt"));
    for name in ["source.mjs", "source.mjs.map", "contract.json", "backend.json", "release.json"] {
        assert!(archived.contains_key(name));
    }
    assert!(archived.keys().all(|name| {
        !name.contains(".sdk")
            && !name.contains(".env")
            && Path::new(name).extension().is_none_or(|extension| extension != "java" && extension != "kt")
    }));
    let manifest: Value = serde_json::from_slice(&archived["release.json"]).unwrap();
    let backend: Value = serde_json::from_slice(&archived["backend.json"]).unwrap();
    assert_eq!(manifest["id"], a.id);
    assert_eq!(backend["id"], a.id);
    assert_eq!(manifest["apps"][0]["id"], "arena");
    assert_eq!(manifest["apps"][0]["sessions"]["default"]["capacity"], 8);
    assert_eq!(manifest["apps"][1]["sessions"]["default"]["capacity"], 16);
    assert_eq!(manifest["profiles"]["small"]["memory_mib"], 512);
    for app in manifest["apps"].as_array().unwrap() {
        assert!(archived.contains_key(app["jar"].as_str().unwrap()));
    }
    assert!(manifest.get("classpath").is_none());
    let encoded = manifest.to_string();
    assert!(!encoded.contains(first.root.path().to_str().unwrap()));
    assert!(!encoded.contains("executable") && !encoded.contains("environment") && !encoded.contains("max_processes"));
}

#[test]
fn dev_classpaths_run_from_a_launcher_whose_digest_covers_every_jar() {
    let mut fixture = Fixture::new();
    fixture.inputs.archive = false;
    let root = fixture.root.path().to_owned();
    fixture.descriptor["apps"][0]["classpath"] = json!([root.join("library.jar"), root.join("generated.jar")]);
    fixture.save_descriptor();
    let first = fixture.publish().unwrap();
    assert!(first.archive.is_none());
    assert!(!root.join(format!("dist/{}.tar.gz", first.id)).exists());
    let lobby = |release: &Release| release.apps.iter().find(|app| app.id == "lobby").unwrap().clone();
    let launcher = first.directory.join(lobby(&first).jar);
    let mut jar = zip::ZipArchive::new(fs::File::open(&launcher).unwrap()).unwrap();
    let mut manifest = String::new();
    jar.by_name("META-INF/MANIFEST.MF").unwrap().read_to_string(&mut manifest).unwrap();
    let classpath = manifest.replace("\r\n ", "");
    let classpath = classpath.lines().find_map(|line| line.strip_prefix("Class-Path: ")).unwrap();
    let expected: Vec<_> = ["lobby.jar", "library.jar", "generated.jar"]
        .iter()
        .map(|jar| format!("../../libs/{}.jar", content_digest(&fs::read(root.join(jar)).unwrap())))
        .collect();
    assert_eq!(classpath.split(' ').collect::<Vec<_>>(), expected);
    assert!(manifest.contains("Main-Class: sample.lobby.Provider"));
    assert!(expected.iter().all(|path| launcher.parent().unwrap().join(path).is_file()));

    write_jar(&root.join("library.jar"), &[("sample/Library.class", class(21, 2))]);
    let second = fixture.publish().unwrap();
    assert_ne!(lobby(&first).sha256, lobby(&second).sha256);
    assert_eq!(first.apps.iter().find(|app| app.id == "arena"), second.apps.iter().find(|app| app.id == "arena"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let inode = |release: &Release, name: &str| fs::metadata(release.directory.join(name)).unwrap().ino();
        let unchanged = expected[0].trim_start_matches("../../");
        assert_eq!(inode(&first, unchanged), inode(&second, unchanged));
    }
}

#[test]
fn deployment_requirements_change_the_release_without_changing_the_jar() {
    let fixture = Fixture::new();
    let initial = fixture.publish().unwrap();
    let config = fixture.inputs.project.join("chunk.toml");
    let mut source = fs::read_to_string(&config).unwrap();
    source.push_str("\n[local.profiles.large]\nmemory_mib=1024\nmax_sessions=1\n");
    fs::write(config, source).unwrap();
    let app = fixture.inputs.project.join("apps/lobby/app.toml");
    fs::write(&app, "[sessions.default]\nmachine_profile='large'\ncapacity=32\n").unwrap();
    let changed = fixture.publish().unwrap();
    assert_ne!(initial.id, changed.id);
    for (before, after) in initial.apps.iter().zip(&changed.apps) {
        assert_eq!(before.sha256, after.sha256);
        assert_eq!(before.jar, after.jar);
    }
    let lobby = changed.apps.iter().find(|app| app.id == "lobby").unwrap();
    assert_eq!(lobby.sessions["default"].machine_profile, "large");
    assert_eq!(lobby.sessions["default"].capacity, 32);
    fs::write(&app, "[sessions.default]\ncapacity=8\n").unwrap();
    let inherited = fixture.publish().unwrap();
    let lobby = inherited.apps.iter().find(|app| app.id == "lobby").unwrap();
    assert_eq!(lobby.sessions["default"].machine_profile, "small");
    assert_eq!(lobby.sessions["default"].capacity, 8);
    fs::write(app, "[sessions.unknown]\ncapacity=8\n").unwrap();
    assert!(fixture.publish().err().unwrap().to_string().contains("unknown session type"));
}

#[test]
fn backend_and_jvm_changes_each_create_a_new_complete_release() {
    let fixture = Fixture::new();
    let initial = fixture.publish().unwrap();
    let source = fixture.inputs.backend.join("source.mjs");
    let original = fs::read(&source).unwrap();
    fs::write(&source, "export function status() { return 2; }").unwrap();
    assert_ne!(initial.id, fixture.publish().unwrap().id);
    fs::write(&source, &original).unwrap();
    write_app_jar(&fixture.root.path().join("lobby.jar"), "lobby", b"changed JVM payload");
    assert_ne!(initial.id, fixture.publish().unwrap().id);
    assert_eq!(fs::read(initial.directory.join("source.mjs")).unwrap(), original);
}

#[test]
fn published_directories_and_archives_are_verified_without_overwrite() {
    let fixture = Fixture::new();
    let release = std::thread::scope(|scope| {
        let concurrent = scope.spawn(|| fixture.publish().unwrap());
        let release = fixture.publish().unwrap();
        assert_eq!(release.id, concurrent.join().unwrap().id);
        release
    });
    let unexpected = release.directory.join("extra.jar");
    fs::write(&unexpected, b"tampered").unwrap();
    assert!(fixture.publish().err().unwrap().to_string().contains("modified"));
    assert_eq!(fs::read(&unexpected).unwrap(), b"tampered");
    fs::remove_file(unexpected).unwrap();
    let archive = release.archive.unwrap();
    fs::rename(&archive, archive.with_extension("saved")).unwrap();
    fs::write(&archive, b"tampered archive").unwrap();
    assert!(fixture.publish().err().unwrap().to_string().contains("modified"));
    assert_eq!(fs::read(&archive).unwrap(), b"tampered archive");
    let other = fixture.root.path().join("other");
    fs::create_dir_all(other.join(&release.id)).unwrap();
    assert!(publish_release(&fixture.inputs, &other).is_err());
    assert_eq!(fs::read_dir(other.join(&release.id)).unwrap().count(), 0);
}

#[test]
fn descriptors_reject_missing_fields_wrong_apps_and_incompatible_java() {
    let mut fixture = Fixture::new();
    let original = fixture.descriptor.clone();
    let mut missing = original.clone();
    missing["apps"][0].as_object_mut().unwrap().remove("jar");
    let mut duplicate = original.clone();
    duplicate["apps"][1] = duplicate["apps"][0].clone();
    let mut extra = original.clone();
    extra["apps"][0]["id"] = json!("unregistered");
    let mut java = original.clone();
    java["java"]["version"] = json!(21);
    let mut unknown = original;
    unknown["java"]["secret"] = json!("not a descriptor field");
    for (descriptor, message) in [
        (missing, "jar"),
        (duplicate, "inventory"),
        (extra, "inventory"),
        (java, "Java 25"),
        (unknown, "unknown field"),
    ] {
        fixture.descriptor = descriptor;
        fixture.save_descriptor();
        let error = fixture.publish().err().unwrap().to_string();
        assert!(error.contains(message), "{error}");
    }
    assert!(!fixture.root.path().join("dist").exists());
}

#[test]
fn releases_reject_incompatible_bytecode_and_missing_main_classes() {
    let fixture = Fixture::new();
    write_jar(
        &fixture.root.path().join("lobby.jar"),
        &[("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\r\nMain-Class: missing.Main\r\n\r\n".to_vec())],
    );
    assert!(fixture.publish().err().unwrap().to_string().contains("Main-Class"));
    write_jar(&fixture.root.path().join("library.jar"), &[("sample/Library.class", class(26, 1))]);
    let bytes = fs::read(fixture.root.path().join("library.jar")).unwrap();
    assert!(jars::Classpath::default().add(&bytes, "library", 25).unwrap_err().to_string().contains("incompatible"));
}

#[test]
fn class_conflicts_use_the_effective_multi_release_definition() {
    let fixture = Fixture::new();
    write_jar(
        &fixture.root.path().join("library.jar"),
        &[
            ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\r\nMulti-Release: true\r\n\r\n".to_vec()),
            ("sample/Library.class", class(21, 1)),
            ("META-INF/versions/23/sample/Library.class", class(23, 2)),
            ("META-INF/versions/26/sample/Library.class", class(26, 3)),
            ("module-info.class", class(21, 1)),
        ],
    );
    write_jar(
        &fixture.root.path().join("generated.jar"),
        &[("sample/Library.class", class(23, 2)), ("module-info.class", class(21, 2))],
    );
    let mut classes = jars::Classpath::default();
    classes.add(&fs::read(fixture.root.path().join("library.jar")).unwrap(), "library", 25).unwrap();
    classes.add(&fs::read(fixture.root.path().join("generated.jar")).unwrap(), "generated", 25).unwrap();
    write_jar(&fixture.root.path().join("generated.jar"), &[("sample/Library.class", class(21, 1))]);
    assert!(
        classes
            .add(&fs::read(fixture.root.path().join("generated.jar")).unwrap(), "generated", 25)
            .unwrap_err()
            .to_string()
            .contains("conflicting class")
    );
}

#[cfg(unix)]
#[test]
fn assets_reject_symlinks_and_nonportable_paths() {
    let fixture = Fixture::new();
    let path = fixture.inputs.project.join("assets/drive:escape.txt");
    fs::write(&path, b"ambiguous name").unwrap();
    assert!(fixture.publish().err().unwrap().to_string().contains("not portable"));
    fs::remove_file(path).unwrap();
    std::os::unix::fs::symlink(fixture.inputs.project.join(".env"), fixture.inputs.project.join("assets/link"))
        .unwrap();
    assert!(fixture.publish().is_err());
    assert!(!fixture.root.path().join("dist").exists());
}

#[test]
fn domains_are_pinned_in_release_identity_and_stale_bindings_require_recompilation() {
    let fixture = Fixture::new();
    let initial = fixture.publish().unwrap();
    let scope = |path: &str| {
        let directory = fixture.inputs.project.join("apps").join(path);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("scope.ts"), "export default defineScope({});").unwrap();
    };
    scope("games");
    assert!(fixture.publish().err().unwrap().to_string().contains("missing the project's domain manifest"));
    let contract_path = fixture.inputs.backend.join("contract.json");
    let mut contract: Value = serde_json::from_slice(&fs::read(&contract_path).unwrap()).unwrap();
    contract["domains"] = json!({
        "version": 1,
        "scopes": {"": {"parent": null}, "games": {"parent": ""}},
        "apps": {"lobby": "", "arena": ""},
        "hooks": {}
    });
    fs::write(&contract_path, serde_json::to_vec(&contract).unwrap()).unwrap();
    let release = fixture.publish().unwrap();
    assert_ne!(initial.id, release.id);
    let backend: Value = serde_json::from_slice(&fs::read(release.directory.join("backend.json")).unwrap()).unwrap();
    assert_eq!(backend["domains"], contract["domains"]);
    scope("games/duels");
    assert!(fixture.publish().err().unwrap().to_string().contains("no longer matches the project"));
    contract["domains"]["scopes"]["games/duels"] = json!({"parent": "games"});
    fs::write(&contract_path, serde_json::to_vec(&contract).unwrap()).unwrap();
    assert_ne!(release.id, fixture.publish().unwrap().id);
}

#[test]
fn session_methods_publish_with_the_runtime_contract_and_reject_stale_jars() {
    let fixture = Fixture::new();
    let path = fixture.inputs.backend.join("contract.json");
    let mut contract: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let method = json!({"app":"lobby","session":"default","name":"announce","arguments":{"type":"object","fields":{}},"result":{"type":"integer"}});
    contract["session_methods"] = json!({"version":1,"methods":[method]});
    fs::write(&path, serde_json::to_vec(&contract).unwrap()).unwrap();
    assert!(fixture.publish().err().unwrap().to_string().contains("missing session method manifest"));
    let mut packaged = method;
    packaged["interface"] = json!("sample.Method");
    packaged["binary_interface"] = json!("sample.Method");
    packaged["function"] = json!("announce");
    let entries = |method: Value| {
        vec![
            ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\r\nMain-Class: sample.lobby.Provider\r\n\r\n".to_vec()),
            ("sample/lobby/Provider.class", class(25, 1)),
            ("sample/Method.class", class(25, 1)),
            ("META-INF/services/dev.chunkzero.runtime.SessionMethodProvider", b"sample.lobby.Provider\n".to_vec()),
            (
                "META-INF/chunk/session-methods.json",
                serde_json::to_vec(&json!({"version":1,"app":"lobby","methods":[method]})).unwrap(),
            ),
        ]
    };
    write_jar(&fixture.root.path().join("lobby.jar"), &entries(packaged.clone()));
    let release = fixture.publish().unwrap();
    let deployment: chunk_contract::Deployment =
        serde_json::from_slice(&fs::read(release.directory.join("backend.json")).unwrap()).unwrap();
    assert_eq!(deployment.contracts.session_methods.unwrap().methods[0].name, "announce");
    packaged["result"] = json!({"type":"string"});
    write_jar(&fixture.root.path().join("lobby.jar"), &entries(packaged));
    assert!(fixture.publish().err().unwrap().to_string().contains("differ from backend contract"));
}

#[test]
fn destination_policies_are_pinned_to_the_published_session_catalog() {
    let fixture = Fixture::new();
    let path = fixture.inputs.backend.join("contract.json");
    let mut contract: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    contract["destinations"] = json!({"version":1,"entries":{"shared/destinations/lobby":{
        "destination":{"key":"main","session_type":"lobby/default","machine_profile":"small"},
        "overflow":"replicate","empty_timeout_seconds":60
    }}});
    fs::write(&path, serde_json::to_vec(&contract).unwrap()).unwrap();
    let release = fixture.publish().unwrap();
    let bundle: Value = serde_json::from_slice(&fs::read(release.directory.join("backend.json")).unwrap()).unwrap();
    assert_eq!(bundle["destinations"], contract["destinations"]);
    contract["destinations"]["entries"]["shared/destinations/lobby"]["empty_timeout_seconds"] = json!(120);
    fs::write(&path, serde_json::to_vec(&contract).unwrap()).unwrap();
    assert_ne!(fixture.publish().unwrap().id, release.id);
    for (field, value) in [("session_type", "lobby/missing"), ("machine_profile", "large")] {
        let mut invalid = contract.clone();
        invalid["destinations"]["entries"]["shared/destinations/lobby"]["destination"][field] = json!(value);
        fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(fixture.publish().is_err());
    }
}

#[test]
fn session_configurations_require_matching_packaged_schemas_and_registered_providers() {
    let fixture = Fixture::new();
    let path = fixture.inputs.backend.join("contract.json");
    let mut contract: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let configuration = json!({"app":"lobby","session":"default","configuration":{"type":"object","fields":{
        "map":{"schema":{"type":"string"}}
    }}});
    contract["session_configurations"] = json!({"version":1,"configurations":[configuration]});
    fs::write(&path, serde_json::to_vec(&contract).unwrap()).unwrap();
    assert!(fixture.publish().err().unwrap().to_string().contains("missing session configuration manifest"));
    let mut packaged = configuration;
    packaged["interface"] = json!("sample.ConfigProvider");
    packaged["binary_interface"] = json!("sample.ConfigProvider");
    packaged["provider"] = json!("sample.lobby.Provider");
    let entries = |configuration: Value, provider: &str| {
        vec![
            ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\r\nMain-Class: sample.lobby.Provider\r\n\r\n".to_vec()),
            ("sample/lobby/Provider.class", class(25, 1)),
            ("sample/ConfigProvider.class", class(25, 1)),
            ("META-INF/services/dev.chunkzero.runtime.SessionProvider", provider.as_bytes().to_vec()),
            (
                "META-INF/chunk/session-configurations.json",
                serde_json::to_vec(&json!({"version":1,"app":"lobby","configurations":[configuration]})).unwrap(),
            ),
        ]
    };
    write_jar(&fixture.root.path().join("lobby.jar"), &entries(packaged.clone(), "sample.lobby.Provider\n"));
    let release = fixture.publish().unwrap();
    let deployment: chunk_contract::Deployment =
        serde_json::from_slice(&fs::read(release.directory.join("backend.json")).unwrap()).unwrap();
    assert_eq!(deployment.contracts.session_configurations.unwrap().configurations[0].session, "default");
    write_jar(&fixture.root.path().join("lobby.jar"), &entries(packaged.clone(), ""));
    assert!(fixture.publish().err().unwrap().to_string().contains("unregistered configured session provider"));
    packaged["configuration"]["fields"]["map"]["schema"] = json!({"type":"integer"});
    write_jar(&fixture.root.path().join("lobby.jar"), &entries(packaged, "sample.lobby.Provider\n"));
    assert!(fixture.publish().err().unwrap().to_string().contains("differ from backend contract"));
}

#[test]
fn destination_only_profiles_are_validated_and_participate_in_release_identity() {
    let fixture = Fixture::new();
    let path = fixture.inputs.backend.join("contract.json");
    let mut contract: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    contract["destinations"] = json!({"version":1,"entries":{"apps/lobby/destinations/large":{
        "destination":{"key":"large","session_type":"lobby/default","machine_profile":"large"},
        "overflow":"replicate","empty_timeout_seconds":60,"creation":{"capacity":32,"configuration":{}}
    }}});
    fs::write(&path, serde_json::to_vec(&contract).unwrap()).unwrap();
    assert!(fixture.publish().err().unwrap().to_string().contains("unknown destination machine profile large"));
    let manifest = fixture.inputs.project.join("chunk.toml");
    let original = fs::read_to_string(&manifest).unwrap();
    fs::write(&manifest, format!("{original}\n[local.profiles.large]\nmemory_mib=1024\nmax_sessions=1\n")).unwrap();
    let first = fixture.publish().unwrap();
    let release: Value = serde_json::from_slice(&fs::read(first.directory.join("release.json")).unwrap()).unwrap();
    assert_eq!(release["profiles"]["large"]["memory_mib"], 1024);
    fs::write(&manifest, format!("{original}\n[local.profiles.large]\nmemory_mib=2048\nmax_sessions=1\n")).unwrap();
    assert_ne!(first.id, fixture.publish().unwrap().id);
}

#[test]
fn authored_apps_require_compiled_policy_and_an_exact_implementation_catalog() {
    let mut fixture = Fixture::new();
    let directory = fixture.inputs.project.join("apps/lobby");
    fs::remove_file(directory.join("app.toml")).unwrap();
    let declaration = "import {defineApp} from '#chunk'; export default defineApp({id:'lobby'});";
    fs::write(directory.join("app.ts"), declaration).unwrap();
    assert!(fixture.publish().err().unwrap().to_string().contains("missing the project's domain manifest"));
    let contract_path = fixture.inputs.backend.join("contract.json");
    let mut contract: Value = serde_json::from_slice(&fs::read(&contract_path).unwrap()).unwrap();
    contract["domains"] = json!({
        "version":1,"scopes":{"":{"parent":null},"lobby":{"parent":""}},
        "apps":{"lobby":"lobby","arena":""},"hooks":{}
    });
    fs::write(&contract_path, serde_json::to_vec(&contract).unwrap()).unwrap();
    fixture.publish().unwrap();
    fs::write(directory.join("app.ts"), declaration.replace("id:'lobby'", "id:'lobby',implementations:{other:{}}"))
        .unwrap();
    assert!(fixture.publish().err().unwrap().to_string().contains("exactly match"));
    fs::write(directory.join("app.ts"), declaration).unwrap();
    fixture.descriptor["apps"][0]["sessions"] = json!(["default", "other"]);
    fixture.save_descriptor();
    assert!(fixture.publish().err().unwrap().to_string().contains("exactly match"));
}
