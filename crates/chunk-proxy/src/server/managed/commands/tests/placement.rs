//! Gateway admission across deployments: reconnects to the session a player left, and moves approved before an
//! activation.

use super::*;
use crate::server::{authentication::Authenticated, transport::Transport};
use crate::{
    PlatformTarget,
    server::{
        Retarget,
        managed::{Assignment, ClaimGuard, activate, claim_destination, moves::next_move, reroute},
    },
};
use chunk_proto::sync::v1::{GatewayMove, SessionDemand};
use chunk_protocol::{
    RemainingBytes,
    versions::v26_2::{
        AcknowledgeConfiguration, ConfigurationAcknowledged, ConfigurationKeepAlive, ConfigurationKeepAliveResponse,
        FinishConfiguration, Handshake, LoginAcknowledged, LoginPluginRequest, LoginPluginResponse, LoginStart,
        LoginSuccess, StartConfiguration,
    },
};
use serde_json::{Value, json};
use std::{
    io,
    sync::{RwLock, atomic::Ordering},
};
use tokio::net::TcpStream;

fn manifest(app: &str) -> Value {
    let hook = |event: &str, export: &str| json!({"domain": "", "event": event, "export": export});
    json!({
        "version": 1,
        "scopes": {"": {"parent": null}, "games": {"parent": ""}, format!("games/{app}"): {"parent": "games"}},
        "apps": {app: format!("games/{app}")},
        "hooks": {
            "shared/domains/hooks/route": hook("player.route", "route"),
            "shared/domains/hooks/login": hook("player.login", "login"),
            "shared/domains/hooks/before": hook("player.beforeMove", "before"),
        },
    })
}

fn demand(app: &str) -> SessionDemand {
    SessionDemand { key: app.into(), session_type: format!("{app}/default"), machine_profile: "local".into() }
}

/// Deployment `a` runs an arena, and `b` and `c` a hub.
fn deployments(fixture: &Fixture, current: &str) {
    let mut placement = fixture.service.placement.lock().unwrap();
    let manifests = [("a", manifest("arena")), ("b", manifest("hub")), ("c", manifest("hub"))];
    placement.manifests = manifests.into_iter().map(|(name, manifest)| (name.to_owned(), manifest)).collect();
    placement.current = Some(current.into());
}

fn retarget(fixture: &Fixture, deployment: &str) -> Retarget {
    Retarget(Arc::new(RwLock::new(fixture.commands.tasks.platform.bind(deployment))))
}

/// The hooks run so far, as deployment and hook name.
fn hooks(fixture: &Fixture) -> Vec<(String, String)> {
    let placement = fixture.service.placement.lock().unwrap();
    let name = |hook: &str| hook.rsplit('/').next().unwrap().to_owned();
    placement.hooks.iter().map(|(deployment, hook, _)| (deployment.clone(), name(hook))).collect()
}

#[tokio::test]
async fn a_login_returned_to_an_earlier_deployments_session_is_admitted_only_there_or_placed_anew() {
    let fixture = Fixture::new().await;
    deployments(&fixture, "b");
    {
        let mut placement = fixture.service.placement.lock().unwrap();
        placement.returns = Some(("a".into(), demand("arena")));
        placement.denying.insert("b".into());
    }
    let current = retarget(&fixture, "b");
    let login = Claim { operation_id: "login".into(), ..fixture.claim.clone() };

    // The current deployment denies every login, but the player returns to the arena, which only the earlier one
    // declares and admits; the current deployment's hooks never run.
    let (mut guard, assignment) = claim_destination(&login, &current).await.unwrap();
    assert_eq!(guard.platform.target.deployment, "a");
    assert_eq!(guard.claim.demand.session_type, "arena/default");
    let mut commands = Commands::new(&guard.platform).await.unwrap();
    commands.bind(&guard.claim, &assignment.identity).unwrap();
    assert_eq!(hooks(&fixture), [("a".to_owned(), "login".to_owned())]);
    {
        let placement = fixture.service.placement.lock().unwrap();
        assert_eq!(placement.hooks[0].2["destination"]["session_type"], "arena/default");
    }
    guard.armed = false;

    // The earlier deployment denies the player, so the login is routed and placed on the current one.
    {
        let mut placement = fixture.service.placement.lock().unwrap();
        placement.denying = ["a".to_owned()].into();
    }
    let login = Claim { operation_id: "denied".into(), ..fixture.claim.clone() };
    let (mut guard, assignment) = claim_destination(&login, &current).await.unwrap();
    assert_eq!((guard.platform.target.deployment.as_str(), assignment.deployment.as_str()), ("b", "b"));
    assert_eq!(guard.claim.demand.session_type, "hub/default");
    {
        let logins = fixture.service.logins.lock().unwrap();
        let denied: Vec<_> = logins.claims.iter().filter(|(operation, _)| operation != "login").collect();
        assert_eq!(denied.len(), 1);
        assert!(denied[0].1.reconnect_session.is_empty() && denied[0].0 == "denied");
        assert!(logins.cancels.is_empty());
    }
    guard.armed = false;
    fixture.close().await;
}

/// Starts `source`'s move to a hub, which `current` approves and places.
fn moving(
    fixture: &Fixture,
    current: &Retarget,
) -> (ClaimGuard, tokio::task::JoinHandle<io::Result<(ClaimGuard, Assignment)>>) {
    let source = ClaimGuard {
        platform: fixture.commands.tasks.platform.bind("a"),
        claim: Claim { demand: demand("arena"), deployment: "a".into(), ..fixture.claim.clone() },
        armed: false,
        failure: None,
    };
    fixture.service.movement.lock().unwrap().pending =
        Some(GatewayMove { operation_id: "move".into(), destination: Some(demand("hub")) });
    fixture.service.publish();
    let (identity, current) = (fixture.identity.clone(), current.clone());
    let watching =
        ClaimGuard { claim: source.claim.clone(), platform: source.platform.clone(), armed: false, failure: None };
    (source, tokio::spawn(async move { next_move(&watching, &identity, 0, &current).await }))
}

#[tokio::test]
async fn a_move_names_its_source_domain_from_the_source_deployment() {
    let fixture = Fixture::new().await;
    deployments(&fixture, "b");
    let (_source, running) = moving(&fixture, &retarget(&fixture, "b"));
    let (mut destination, assignment) =
        tokio::time::timeout(Duration::from_secs(5), running).await.unwrap().unwrap().unwrap();
    destination.armed = false;
    assert_eq!(assignment.deployment, "b");
    // The destination deployment declares no arena app, so its own manifest can't name the source's domain.
    {
        let placement = fixture.service.placement.lock().unwrap();
        let (_, _, payload) = placement.hooks.iter().find(|(_, hook, _)| hook.ends_with("before")).unwrap();
        assert_eq!(payload["sourceDomain"], "games/arena");
    }
    fixture.close().await;
}

#[tokio::test]
async fn a_move_approved_before_an_activation_is_approved_again_by_the_new_deployment() {
    let fixture = Fixture::new().await;
    deployments(&fixture, "c");
    fixture.service.placement.lock().unwrap().denying.insert("c".into());
    let current = retarget(&fixture, "b");
    let (_source, running) = moving(&fixture, &current);
    let movement = fixture.service.movement.clone();
    tokio::time::timeout(Duration::from_secs(5), async {
        while movement.lock().unwrap().attempts == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // The gateway learns of the activation after core refused the move placed in `b`.
    current.replace(PlatformTarget { deployment: "c".into(), ..current.platform().target });
    tokio::time::timeout(Duration::from_secs(5), async {
        while movement.lock().unwrap().failure.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // `c` denied the move, so it was never placed there.
    let deployments: Vec<_> = hooks(&fixture).into_iter().map(|(deployment, _)| deployment).collect();
    assert_eq!(deployments.first().map(String::as_str), Some("b"));
    assert_eq!(deployments.last().map(String::as_str), Some("c"));
    assert_eq!(movement.lock().unwrap().attempts, 1);
    running.abort();
    fixture.close().await;
}

#[tokio::test]
async fn a_move_reserved_in_an_earlier_deployment_is_approved_there_before_it_is_accepted() {
    let fixture = Fixture::new().await;
    deployments(&fixture, "c");
    {
        let mut placement = fixture.service.placement.lock().unwrap();
        placement.returns = Some(("b".into(), demand("hub")));
        placement.reserved = placement.returns.clone();
        placement.denying.insert("b".into());
    }
    let (_source, running) = moving(&fixture, &retarget(&fixture, "c"));
    let movement = fixture.service.movement.clone();
    tokio::time::timeout(Duration::from_secs(5), async {
        while movement.lock().unwrap().failure.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // `b`, which holds the reservation, denied the move, so the reservation was withdrawn; `c` never saw it.
    assert!(hooks(&fixture).iter().all(|(deployment, _)| deployment == "b"));
    running.abort();
    fixture.close().await;
}

#[tokio::test]
async fn a_reserved_move_completes_in_its_deployment_though_the_current_one_dropped_its_app() {
    let fixture = Fixture::new().await;
    deployments(&fixture, "c");
    {
        let mut placement = fixture.service.placement.lock().unwrap();
        placement.manifests.insert("c".into(), manifest("other"));
        placement.denying.insert("c".into());
        placement.returns = Some(("b".into(), demand("hub")));
        placement.reserved = placement.returns.clone();
    }
    let (_source, running) = moving(&fixture, &retarget(&fixture, "c"));
    let (mut destination, assignment) =
        tokio::time::timeout(Duration::from_secs(5), running).await.unwrap().unwrap().unwrap();
    destination.armed = false;
    assert_eq!(assignment.deployment, "b");
    assert!(hooks(&fixture).iter().all(|(deployment, _)| deployment == "b"));
    fixture.close().await;
}

#[tokio::test]
async fn a_login_whose_session_retired_while_claiming_is_claimed_again_under_a_new_operation() {
    let fixture = Fixture::new().await;
    deployments(&fixture, "b");
    fixture.service.retiring.store(true, Ordering::SeqCst);
    let login = Claim { operation_id: "login".into(), ..fixture.claim.clone() };
    let (mut guard, _) = claim_destination(&login, &retarget(&fixture, "b")).await.unwrap();
    guard.armed = false;
    let claims = fixture.service.logins.lock().unwrap().claims.clone();
    assert_eq!(claims.len(), 2);
    assert_eq!(claims[0].0, "login");
    assert_ne!(claims[1].0, "login");
    assert_eq!(guard.claim.operation_id, claims[1].0);
    fixture.close().await;
}

#[tokio::test]
async fn a_move_whose_session_retired_before_activation_is_recorded_failed_and_its_player_routed_as_a_login() {
    let fixture = Fixture::new().await;
    deployments(&fixture, "b");
    let current = retarget(&fixture, "b");
    let (_source, running) = moving(&fixture, &current);
    let (destination, _) = tokio::time::timeout(Duration::from_secs(5), running).await.unwrap().unwrap().unwrap();
    fixture.service.retiring.store(true, Ordering::SeqCst);
    assert!(!activate(&destination).await.unwrap());

    let (mut login, assignment) = reroute(destination, &current).await.unwrap();
    login.armed = false;
    assert_eq!(assignment.deployment, "b");
    assert!(login.claim.source.is_none());
    assert_eq!(login.claim.connection_id, fixture.claim.connection_id);
    let failure = fixture.service.movement.lock().unwrap().failure.clone();
    assert_eq!(failure, Some(("move".into(), "destination session retired".into())));
    // The retired claim was withdrawn before its player's login was claimed.
    {
        let logins = fixture.service.logins.lock().unwrap();
        assert_eq!(logins.cancels, ["move"]);
        assert_eq!(logins.claims.last().map(|(operation, _)| operation), Some(&login.claim.operation_id));
    }
    fixture.close().await;
}

/// Plays a JVM's player listener for one connection: its delivery login, then, when `finish`, the end of configuration.
/// Returns the connection, which stays open until dropped.
async fn jvm_accepts(listener: &tokio::net::TcpListener, profile: &LoginSuccess, finish: bool) -> Transport<TcpStream> {
    let (socket, _) = listener.accept().await.unwrap();
    let mut jvm = Transport::new(socket);
    decode_packet::<Handshake>(&jvm.read_frame(4096).await.unwrap()).unwrap();
    decode_packet::<LoginStart>(&jvm.read_frame(4096).await.unwrap()).unwrap();
    let challenge = LoginPluginRequest {
        message_id: VarInt(1),
        channel: McString::new("chunk:delivery").unwrap(),
        data: RemainingBytes::new(vec![]).unwrap(),
    };
    jvm.write_packet(&challenge).await.unwrap();
    decode_packet::<LoginPluginResponse>(&jvm.read_frame(4096).await.unwrap()).unwrap();
    jvm.write_packet(profile).await.unwrap();
    decode_packet::<LoginAcknowledged>(&jvm.read_frame(4096).await.unwrap()).unwrap();
    decode_packet::<ConfigurationClientInformation>(&jvm.read_frame(4096).await.unwrap()).unwrap();
    if finish {
        jvm.write_packet(&FinishConfiguration).await.unwrap();
        decode_packet::<AcknowledgeConfiguration>(&jvm.read_frame(4096).await.unwrap()).unwrap();
    }
    jvm
}

/// Reads frames from `client`, answering configuration keepalives, until one is a `P`.
async fn client_reads<P: Packet>(client: &mut Transport<DuplexStream>) {
    loop {
        let frame = client.read_frame(16384).await.unwrap();
        match VarInt::decode(&mut frame.as_ref()).unwrap().0 {
            id if id == P::ID => return,
            id if id == ConfigurationKeepAlive::ID => {
                let keep_alive_id = decode_packet::<ConfigurationKeepAlive>(&frame).unwrap().keep_alive_id;
                client.write_packet(&ConfigurationKeepAliveResponse { keep_alive_id }).await.unwrap();
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn a_move_whose_destination_retires_during_configuration_keeps_the_player_connected_on_a_new_login() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Fixture::new().await;
    {
        let mut placement = fixture.service.placement.lock().unwrap();
        placement.manifests.insert("deployment".into(), manifest("hub"));
        placement.endpoint = Some(listener.local_addr().unwrap().to_string());
    }
    fixture.service.logins.lock().unwrap().retired_operations.insert("move".into());
    let current = retarget(&fixture, "deployment");
    let (client, public) = tokio::io::duplex(65_536);
    let profile = LoginSuccess {
        session_id: chunk_protocol::Uuid([2; 16]),
        uuid: chunk_protocol::Uuid([1; 16]),
        username: McString::new("Alex").unwrap(),
        properties: chunk_protocol::BoundedArray::new(vec![]).unwrap(),
    };
    // Reservations name protocol 0.
    let authenticated =
        Authenticated { protocol_version: 0, profile: profile.clone(), transport: Transport::new(public) };
    let serving = {
        let current = current.clone();
        tokio::spawn(async move {
            Box::pin(crate::server::managed::serve(authenticated, &current, Duration::from_secs(30))).await
        })
    };
    let mut client = Transport::new(client);
    let settings = ConfigurationClientInformation {
        locale: McString::new("en_US").unwrap(),
        view_distance: 8,
        chat_flags: VarInt(0),
        chat_colors: true,
        skin_parts: 127,
        main_hand: VarInt(1),
        enable_text_filtering: false,
        enable_server_listing: true,
        particle_status: ConfigurationClientInformationParticleStatus::All,
    };
    client.write_packet(&settings).await.unwrap();

    // Core's topic shows the player's login once it is claimed.
    let login = loop {
        let claimed = fixture.service.logins.lock().unwrap().claims.first().cloned();
        if let Some(login) = claimed {
            break login;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    fixture.service.claim.lock().unwrap().operation.clone_from(&login.0);
    fixture.service.publish();
    let ((), source) = tokio::join!(
        async {
            client_reads::<FinishConfiguration>(&mut client).await;
            client.write_packet(&AcknowledgeConfiguration).await.unwrap();
        },
        jvm_accepts(&listener, &profile, true)
    );

    // Core queues a move, which the gateway prepares, and the client starts configuring for it.
    fixture.service.movement.lock().unwrap().pending =
        Some(GatewayMove { operation_id: "move".into(), destination: Some(demand("hub")) });
    fixture.service.publish();
    client_reads::<StartConfiguration>(&mut client).await;

    // The destination's session retires while the client configures: core ends the move.
    {
        let mut movement = fixture.service.movement.lock().unwrap();
        movement.pending = None;
        movement.vanished = Some("move".into());
    }
    fixture.sync().await;
    client.write_packet(&ConfigurationAcknowledged).await.unwrap();

    // The player's new login reaches a JVM on the connection that was moving.
    let replacement =
        tokio::time::timeout(Duration::from_secs(10), jvm_accepts(&listener, &profile, false)).await.unwrap();
    let (claims, cancels) = {
        let logins = fixture.service.logins.lock().unwrap();
        (logins.claims.clone(), logins.cancels.clone())
    };
    assert_eq!(claims.len(), 2, "one login for the player, and one fresh login after the retirement");
    assert_ne!(claims[1].0, login.0);
    assert_eq!(claims[1].1.connection_id, login.1.connection_id);
    assert_eq!(claims[1].1.player, login.1.player);
    assert_eq!(cancels, [login.0, "move".to_owned()], "the source, then the retired destination");
    {
        let movement = fixture.service.movement.lock().unwrap();
        assert_eq!(movement.failure, Some(("move".into(), "destination session retired".into())));
        assert_eq!(movement.reports, 1);
    }
    // The login, the move twice (before cutover and after it), and the new login.
    assert_eq!(fixture.service.activations.load(Ordering::SeqCst), 4);
    assert!(!serving.is_finished());
    serving.abort();
    drop((source, replacement, client));
    fixture.close().await;
}
