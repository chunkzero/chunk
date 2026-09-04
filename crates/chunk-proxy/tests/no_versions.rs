#![cfg(not(feature = "mc-26-1"))]

use std::io;

use chunk_proxy::{Config, Proxy};

#[tokio::test]
async fn no_version_features_prevents_startup() {
    assert!(chunk_protocol::versions::SUPPORTED.is_empty());
    let result = Proxy::bind("127.0.0.1:0".parse().unwrap(), Config::default()).await;
    let error = result.err().expect("a proxy without version support cannot start");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("no Minecraft version enabled"));
}
