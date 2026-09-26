use super::*;
use crate::publication;

fn digest(path: &Path) -> ArchiveDigest {
    let bytes = fs::read(path).unwrap();
    ArchiveDigest { sha256: content_digest(&bytes), size: bytes.len() as u64 }
}

fn unpacked(fixture: &Fixture) -> (Release, PathBuf) {
    let release = fixture.publish().unwrap();
    let archive = release.archive.clone().unwrap();
    let directory = fixture.root.path().join("environment/release");
    unpack_release(&archive, &digest(&archive), &directory, &UnpackLimits::default()).unwrap();
    (release, directory)
}

#[test]
fn unpacked_releases_verify_and_reject_tampering() {
    let fixture = Fixture::new();
    let (release, directory) = unpacked(&fixture);
    assert_eq!(verify_release(&directory).unwrap().id, release.id);

    let asset = directory.join("apps/lobby/assets/map.txt");
    fs::write(&asset, b"tampered").unwrap();
    assert!(verify_release(&directory).unwrap_err().to_string().contains("differ from its ID"));
    fs::write(&asset, b"lobby").unwrap();
    let backend = directory.join("backend.json");
    let mut bundle: Value = serde_json::from_slice(&fs::read(&backend).unwrap()).unwrap();
    bundle["source"] = json!("export function status() { return 2; }");
    fs::write(&backend, serde_json::to_vec(&bundle).unwrap()).unwrap();
    assert!(verify_release(&directory).unwrap_err().to_string().contains("backend.json differs"));
}

#[test]
fn verification_rejects_consistently_signed_invalid_releases() {
    let fixture = Fixture::new();
    let (_, directory) = unpacked(&fixture);
    let path = directory.join("contract.json");
    let mut contract: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    contract["domains"] = json!({"version":1,"scopes":{"":{"parent":null}},"apps":{"lobby":""},"hooks":{}});
    fs::write(&path, serde_json::to_vec(&contract).unwrap()).unwrap();

    let mut files = publication::Files::new();
    publication::collect(&directory, "", &mut files).unwrap();
    let metadata: Metadata = serde_json::from_slice(&files["release.json"]).unwrap();
    files.remove("backend.json");
    files.insert("release.json".into(), serde_json::to_vec(&metadata).unwrap());
    let id = publication::digest(files.iter().map(|(name, bytes)| (name.as_str(), bytes.as_slice())));
    let signed = serde_json::to_vec(&Manifest { id: &id, metadata: &metadata }).unwrap();
    fs::write(directory.join("release.json"), signed).unwrap();
    let source = String::from_utf8(files["source.mjs"].clone()).unwrap();
    let mut backend = verify::deployment(source, serde_json::from_value(contract).unwrap());
    backend.id = id;
    fs::write(directory.join("backend.json"), serde_json::to_vec(&backend).unwrap()).unwrap();

    let error = verify_release(&directory).unwrap_err().to_string();
    assert!(error.contains("domain manifest app bindings"), "{error}");
}

#[test]
fn unpack_rejects_escaping_deep_and_oversized_paths() {
    let root = tempfile::tempdir().unwrap();
    let unpack = |names: &[&str], limits: UnpackLimits| {
        let archive = root.path().join("release.tar.gz");
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default()));
        for name in names {
            let mut header = tar::Header::new_gnu();
            header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
            header.set_size(1);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder.append(&header, b"x".as_slice()).unwrap();
        }
        fs::write(&archive, builder.into_inner().unwrap().finish().unwrap()).unwrap();
        let directory = root.path().join("unpacked/release");
        let error = unpack_release(&archive, &digest(&archive), &directory, &limits).unwrap_err().to_string();
        assert!(!root.path().join("unpacked").read_dir().unwrap().any(|_| true), "{error}");
        error
    };
    assert!(unpack(&["../escape.txt"], UnpackLimits::default()).contains("not portable"));
    assert!(unpack(&[&format!("{}f", "d/".repeat(18))], UnpackLimits::default()).contains("nesting limit"));
    let limits = UnpackLimits { entries: 4, bytes: 1024 };
    assert!(unpack(&["a/b/c/1", "a/b/c/2"], limits).contains("exceeds unpack limits"));
}
