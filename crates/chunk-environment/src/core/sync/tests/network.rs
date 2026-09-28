//! Core's network listener and the credentials other machines present on it.

use super::{
    commands::{Arrived, Gateway, decoded, failure},
    *,
};
use chunk_control::MachineKind;
use chunk_proto::sync::v1::{CommandStarted, GatewayDeployment};
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
async fn the_deployment_topic_follows_the_current_release_for_gateways_until_revoked() {
    let fixture = Fixture::start().await;
    fixture.control.add_machine("remote", MachineKind::Gateway).unwrap();
    let credential = Issuer::new("test", None, &fixture.cli).machine(MachineKind::Gateway, "remote");
    let mut client = CoreClient::connect(fixture.network.clone()).await.unwrap();
    let subscription = SubscribeRequest { topic: "deployment".into(), ..SubscribeRequest::default() };
    let mut denied = fixture.client.clone().subscribe(authorized(subscription.clone(), &fixture.cli)).await.unwrap();
    assert_eq!(next(denied.get_mut()).await.error.unwrap().code(), Code::Denied);

    let mut updates = client.subscribe(authorized(subscription, &credential)).await.unwrap().into_inner();
    let first = next(&mut updates).await;
    assert!(first.snapshot && first.upserts.is_empty() && !first.stream.is_empty());
    fixture.control.activate_release(runtime::release()).unwrap();
    let current = next(&mut updates).await;
    let value = GatewayDeployment { deployment: "test".into() }.encode_to_vec();
    assert!(current.snapshot && current.position.is_some());
    assert_eq!(current.upserts, [Entry { key: "current".into(), state: Some(State::Value(value)) }]);

    fixture.control.revoke_machine("remote", MachineKind::Gateway).unwrap();
    assert_eq!(next(&mut updates).await.error.unwrap().code(), Code::Stopped);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoking_a_gateway_machine_stops_its_command_subscription_and_cancels_the_command() {
    let (fixture, jvm) = runtime::with_jvm().await;
    fixture.control.add_machine("proxy", MachineKind::Gateway).unwrap();
    let credential = Issuer::new("test", None, &fixture.cli).machine(MachineKind::Gateway, "proxy");
    let (updates, gateway) = Gateway::follow_own(&fixture, credential, "proxy").await;
    let mut arrived = Arrived { fixture, updates, gateway, jvm };
    arrived.arrive().await;
    let gateway = arrived.gateway.clone();
    let operation = gateway.prepare().await;
    decoded::<CommandStarted>(&gateway.say(&operation, "say write").await);
    let mut effects = gateway.follow(&operation).await;
    next(&mut effects).await;

    arrived.fixture.control.revoke_machine("proxy", MachineKind::Gateway).unwrap();
    let mut last = next(&mut effects).await;
    while last.error.is_none() {
        last = next(&mut effects).await;
    }
    assert_eq!(failure(&last), Some(Code::Stopped));
    // `say write` writes 300 ms after it starts unless it's cancelled.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let cli = arrived.fixture.cli.clone();
    assert_eq!(arrived.fixture.call(&cli, "", "get", "null").await.outcome, Some(Outcome::Result(b"0".to_vec())));
    arrived.stop().await;
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
