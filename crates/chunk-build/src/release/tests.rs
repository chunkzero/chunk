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
            "version":3, "java":{"version":25,"executable":root.path().join("jdk/bin/java")},
            "apps":[
                {"id":"lobby","jar":root.path().join("lobby.jar"),"java_version":25,"sessions":["default"]},
                {"id":"arena","jar":root.path().join("arena.jar"),"java_version":25,"sessions":["default"]}
            ]
        });
        let jvm_descriptor = root.path().join("artifacts.json");
        let fixture = Self { root, inputs: ReleaseInputs { project, backend, jvm_descriptor }, descriptor };
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
    let compressed = fs::read(&a.archive).unwrap();
    assert_eq!(compressed, fs::read(&b.archive).unwrap());
    assert_eq!(&compressed[..8], &[0x1f, 0x8b, 8, 0, 0, 0, 0, 0]);
    assert_eq!(compressed[9], 255);
    assert_eq!(a.archive.parent(), a.directory.parent());
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
    fs::rename(&release.archive, release.archive.with_extension("saved")).unwrap();
    fs::write(&release.archive, b"tampered archive").unwrap();
    assert!(fixture.publish().err().unwrap().to_string().contains("modified"));
    assert_eq!(fs::read(&release.archive).unwrap(), b"tampered archive");
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
    assert!(
        jars::Classpath::default().add(&bytes, "library", 25, false).unwrap_err().to_string().contains("incompatible")
    );
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
    classes.add(&fs::read(fixture.root.path().join("library.jar")).unwrap(), "library", 25, false).unwrap();
    classes.add(&fs::read(fixture.root.path().join("generated.jar")).unwrap(), "generated", 25, false).unwrap();
    write_jar(&fixture.root.path().join("generated.jar"), &[("sample/Library.class", class(21, 1))]);
    assert!(
        classes
            .add(&fs::read(fixture.root.path().join("generated.jar")).unwrap(), "generated", 25, false)
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
