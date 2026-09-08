use super::*;

#[tokio::test]
async fn cleanup_waits_for_pending_launches_even_when_another_runtime_fails() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.launch");
    let second = directory.path().join("second.launch");
    fs::write(&first, b"").unwrap();
    fs::write(&second, b"").unwrap();
    fs::write(
        directory.path().join("failed.json"),
        serde_json::to_vec(&chunk_runtime::RuntimeConnection {
            endpoint: "invalid".into(),
            token: "test".into(),
            identity: chunk_proto::v1::ProcessIdentity::default(),
        })
        .unwrap(),
    )
    .unwrap();
    let exits = async {
        sleep(Duration::from_millis(50)).await;
        fs::write(first.with_extension("exit"), b"stopped").unwrap();
        sleep(Duration::from_millis(50)).await;
        fs::write(second.with_extension("exit"), b"stopped").unwrap();
    };
    let cleanup = async {
        assert!(stop_runtimes(directory.path()).await.is_err());
        assert!(first.with_extension("exit").exists());
        assert!(second.with_extension("exit").exists());
    };
    tokio::join!(exits, cleanup);
}
