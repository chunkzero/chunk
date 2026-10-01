//! Gateway admission across deployments: reconnects to the session a player left, and moves approved before an
//! activation.

use super::*;
use crate::{
    PlatformTarget,
    server::{
        Retarget,
        managed::{Assignment, ClaimGuard, claim_destination, moves::next_move},
    },
};
use chunk_proto::sync::v1::{GatewayMove, SessionDemand};
use serde_json::{Value, json};
use std::{io, sync::RwLock};

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
