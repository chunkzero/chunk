use super::*;

fn digest(path: &Path) -> ArchiveDigest {
    let bytes = fs::read(path).unwrap();
    ArchiveDigest { sha256: content_digest(&bytes), size: bytes.len() as u64 }
}

#[test]
fn unpacked_releases_verify_and_reject_tampering() {
    let fixture = Fixture::new();
    let release = fixture.publish().unwrap();
    let archive = release.archive.unwrap();
    let directory = fixture.root.path().join("environment/release");
    unpack_release(&archive, &digest(&archive), &directory, &UnpackLimits::default()).unwrap();
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
fn unpack_rejects_paths_escaping_the_directory() {
    let root = tempfile::tempdir().unwrap();
    let archive = root.path().join("release.tar.gz");
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default()));
    let mut header = tar::Header::new_gnu();
    header.as_gnu_mut().unwrap().name[..13].copy_from_slice(b"../escape.txt");
    header.set_size(7);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    builder.append(&header, b"escaped".as_slice()).unwrap();
    fs::write(&archive, builder.into_inner().unwrap().finish().unwrap()).unwrap();
    let directory = root.path().join("unpacked/release");
    let error = unpack_release(&archive, &digest(&archive), &directory, &UnpackLimits::default()).unwrap_err();
    assert!(error.to_string().contains("not portable"), "{error}");
    assert!(!directory.exists() && !root.path().join("unpacked/escape.txt").exists());
}
