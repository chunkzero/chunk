use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::Packs;

#[tokio::test]
async fn only_the_packs_of_staged_releases_are_served() {
    let directory = tempfile::tempdir().unwrap();
    let store = chunk_build::assets::Store::new(directory.path());
    let sha256 = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
    let (pack, other): (&[u8], &[u8]) = (b"PK pack", b"a world");
    store.insert(&sha256(pack), pack).unwrap();
    store.insert(&sha256(other), other).unwrap();
    let packs = Packs::start(directory.path().into()).await.unwrap();
    let blob = chunk_contract::PackBlob { sha256: sha256(pack), sha1: "0".repeat(40), size: 7 };
    let assets = chunk_control::DeploymentAssets {
        packs: [("base".into(), blob)].into(),
        ..chunk_control::DeploymentAssets::default()
    };
    packs.serve(&assets);

    let get = |sha256: String| {
        let url = format!("{}{sha256}", packs.url_prefix());
        async move {
            let rest = url.strip_prefix("http://").unwrap();
            let (address, path) = rest.split_at(rest.find('/').unwrap());
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            let request = format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            response
        }
    };
    let served = get(sha256(pack)).await;
    assert!(served.starts_with("HTTP/1.1 200") && served.ends_with("PK pack"), "{served}");
    assert!(get(sha256(other)).await.starts_with("HTTP/1.1 404"));
}
