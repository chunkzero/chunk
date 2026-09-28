//! Core's network listener and the credentials other machines present on it.

use super::*;
use chunk_control::MachineKind;
use tonic::transport::server::TcpConnectInfo;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_machine_authenticates_over_the_network_until_revoked() {
    let fixture = Fixture::start().await;
    fixture.control.add_machine("remote", MachineKind::Gateway).unwrap();
    let issuer = Issuer::new("test", None, &fixture.cli);
    let credential = issuer.machine(MachineKind::Gateway, "remote");
    let mut client = CoreClient::connect(fixture.network.clone()).await.unwrap();
    let subscription = SubscribeRequest { topic: "gateway/remote".into(), ..SubscribeRequest::default() };
    let mut updates = client.subscribe(authorized(subscription, &credential)).await.unwrap().into_inner();
    let first = next(&mut updates).await;
    assert!(first.error.is_none() && !first.stream.is_empty());
    let read =
        CallRequest { method: "get".into(), arguments: "null".into(), deployment: "test".into(), ..Default::default() };
    for credential in [credential.as_str(), issuer.operator()] {
        let response = client.call(authorized(read.clone(), credential)).await.unwrap().into_inner();
        assert_eq!(response.outcome, Some(Outcome::Result(b"0".to_vec())));
    }

    fixture.control.revoke_machine("remote", MachineKind::Gateway).unwrap();
    let last = loop {
        let update = next(&mut updates).await;
        if let Some(error) = update.error {
            break error;
        }
    };
    assert_eq!(last.code(), Code::Stopped);
    let status = client.call(authorized(read, &credential)).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_credential_holds_only_from_loopback_and_the_operator_credential_from_any_peer() {
    let fixture = Fixture::start().await;
    let issuer = Issuer::new("test", None, &fixture.cli);
    let credentials = auth::Credentials {
        gateways: fixture.gateways.clone(),
        cli: fixture.cli.clone(),
        issuer: issuer.clone(),
        control: fixture.control.clone(),
    };
    let class = |peer: &str, credential: &str| {
        let mut request = authorized((), credential);
        let remote_addr = Some(peer.parse().unwrap());
        request.extensions_mut().insert(TcpConnectInfo { local_addr: None, remote_addr });
        credentials.authenticate(&request).map(|principal| principal.class).map_err(|status| status.code())
    };
    assert_eq!(class("127.0.0.1:1", &fixture.cli), Ok(auth::Class::Cli));
    assert_eq!(class("[::ffff:127.0.0.1]:1", &fixture.cli), Ok(auth::Class::Cli));
    assert_eq!(class("10.0.0.5:1", &fixture.cli), Err(tonic::Code::Unauthenticated));
    assert_eq!(class("10.0.0.5:1", issuer.operator()), Ok(auth::Class::Cli));
    fixture.stop().await;
}
