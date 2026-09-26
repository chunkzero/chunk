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
fn unpack_rejects_unsafe_archives_without_leaving_anything_behind() {
    use tar::EntryType::{Regular, Symlink};
    let root = tempfile::tempdir().unwrap();
    let (archive, parent) = (root.path().join("release.tar.gz"), root.path().join("unpacked"));
    let destination = parent.join("release");
    let deep = format!("{}f", "d/".repeat(18));
    let long = "a".repeat(5000);
    let small = UnpackLimits { entries: 4, bytes: 1024 };
    let default = UnpackLimits::default;
    for (entries, limits, tamper, existing, expected) in [
        (vec![("../escape", Regular, 1)], default(), false, false, "not portable"),
        (vec![(deep.as_str(), Regular, 1)], default(), false, false, "nesting limits"),
        (vec![("a/b/c/1", Regular, 1), ("a/b/c/2", Regular, 1)], small, false, false, "exceeds unpack limits"),
        (vec![("big", Regular, 2048)], UnpackLimits { entries: 4, bytes: 1024 }, false, false, "exceeds unpack"),
        (vec![("link", Symlink, 0)], default(), false, false, "other than regular files"),
        (vec![("f", Regular, 1), ("f", Regular, 1)], default(), false, false, "twice"),
        (vec![("f", Regular, 1), ("f/g", Regular, 1)], default(), false, false, "as a directory"),
        (vec![(long.as_str(), Regular, 1)], default(), false, false, "name exceeds limit"),
        (vec![("f", Regular, 1)], default(), true, false, "expected size and SHA-256"),
        (vec![("f", Regular, 1)], default(), false, true, "exists"),
    ] {
        let _ = fs::remove_dir_all(&parent);
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default()));
        for (name, kind, size) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(size);
            header.set_mode(0o644);
            header.set_entry_type(kind);
            header.set_link_name("../../outside").unwrap();
            let data = vec![b'x'; usize::try_from(size).unwrap()];
            if name.len() > 100 {
                builder.append_data(&mut header, name, data.as_slice()).unwrap();
            } else {
                header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
                header.set_cksum();
                builder.append(&header, data.as_slice()).unwrap();
            }
        }
        fs::write(&archive, builder.into_inner().unwrap().finish().unwrap()).unwrap();
        let mut expected_digest = digest(&archive);
        if tamper {
            expected_digest.sha256 = content_digest(b"other");
        }
        if existing {
            fs::create_dir_all(&destination).unwrap();
            fs::write(destination.join("marker"), b"kept").unwrap();
        }
        let error = unpack_release(&archive, &expected_digest, &destination, &limits).unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
        let left: Vec<_> =
            fs::read_dir(&parent).into_iter().flatten().map(|entry| entry.unwrap().file_name()).collect();
        if existing {
            assert_eq!(fs::read(destination.join("marker")).unwrap(), b"kept");
            assert_eq!(fs::read_dir(&destination).unwrap().count(), 1);
            assert_eq!(left, ["release"]);
        } else {
            assert!(left.is_empty(), "{error}: {left:?}");
            assert!(!tamper || !parent.exists());
        }
    }
}
