//! Retiring a stopped release whose sessions have players and whose backend version holds a scheduled job.

use super::*;
use crate::{Core, managed::Managed};
use chunk_backend::{Call, DeploymentId};
use chunk_proto::{
    control::v1::{ClaimRequest, Identity, SessionDemand},
    sync::v1::NodePhase,
};
use std::sync::OnceLock;

fn hold(deployment: &str) -> Call {
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() * 1000 + 3_600_000;
    Call {
        deployment: DeploymentId::new(deployment).unwrap(),
        function: "hold".into(),
        arguments: serde_json::json!({"at": at}).into(),
        caller: serde_json::json!({"player": "alice"}).into(),
    }
}

fn login() -> ClaimRequest {
    ClaimRequest {
        operation_id: "login".into(),
        proxy_id: "proxy".into(),
        connection_id: "connection".into(),
        identity: Some(Identity {
            uuid: "00000000-0000-0000-0000-000000000001".into(),
            username: "player".into(),
            properties: vec![],
        }),
        demand: Some(SessionDemand {
            key: "lobby".into(),
            session_type: "lobby/default".into(),
            machine_profile: "small".into(),
        }),
        ..ClaimRequest::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_occupied_stopped_release_with_a_backend_job_is_retired_without_slot_pressure() {
    let mut harness = Harness::new().await;
    harness.deploy("dep_a", harness.valid());
    let core = Core::start(harness.core(), || {}).await.unwrap();
    let (gateway, lease) = (OnceLock::new(), watch::Sender::new(crate::managed::Lease::Waiting));
    let managed = Managed::new(
        &harness.management_config(),
        lease,
        crate::managed::Registration::local("env_test"),
        &harness.state(),
        &core,
        &gateway,
        None,
    );
    let checks = async {
        harness.expect(1, "dep_a", DeploymentState::InProgress).await;
        harness.expect(1, "dep_a", DeploymentState::Active).await;

        // A player holds a claim on dep_a's session, and dep_a's version holds a job.
        let control = core.control().unwrap();
        let login = login();
        let mut claiming = tokio::spawn({
            let control = control.clone();
            async move { control.claim(login).await }
        });
        tokio::time::timeout(Duration::from_secs(30), async {
            while control.online_players().unwrap() == 0 {
                assert!(!claiming.is_finished(), "{:?}", (&mut claiming).await);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the claim reserves a place");
        let backend = core.backend().unwrap();
        backend.mutate("hold-dep_a".into(), hold("dep_a")).await.unwrap();

        // dep_b replaces dep_a and stops it, though it has a player and a job, so the backend fences dep_a at once.
        harness.deploy_stopping("dep_b", harness.valid());
        harness.expect(2, "dep_b", DeploymentState::InProgress).await;
        harness.expect(2, "dep_b", DeploymentState::Active).await;
        harness.released("dep_a").await;
        assert!(harness.serves("dep_b").await);
        claiming.abort();
    };
    let mut running = Box::pin(managed.run());
    tokio::select! {
        error = &mut running => panic!("{error}"),
        () = checks => {}
    }
    drop(running);
    core.stop(|| {}).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn releasable_deployments_free_the_slots_so_an_occupied_older_one_is_not_forced_out() {
    let harness = Harness::new().await;
    let core = Core::start(harness.core(), || {}).await.unwrap();
    let (gateway, lease) = (OnceLock::new(), watch::Sender::new(crate::managed::Lease::Waiting));
    let registration = crate::managed::Registration::local("env_test");
    let managed =
        Managed::new(&harness.management_config(), lease, registration, &harness.state(), &core, &gateway, None);
    let desired = |id: &str| AttachResponse {
        deployment_id: id.into(),
        release: Some(harness.valid()),
        ..AttachResponse::default()
    };
    let cancel = CancellationToken::new();
    assert!(managed.deploy(&desired("dep_a"), &cancel).await.unwrap());

    // A player holds dep_a's session, and dep_a's version holds a job.
    let control = core.control().unwrap();
    let claiming = tokio::spawn({
        let control = control.clone();
        async move { control.claim(login()).await }
    });
    tokio::time::timeout(Duration::from_secs(30), async {
        while control.online_players().unwrap() == 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the claim reserves a place");
    let backend = core.backend().unwrap();
    backend.mutate("hold-dep_a".into(), hold("dep_a")).await.unwrap();

    // dep_b replaces dep_a, and fourteen releasable versions fill the rest of the backend's sixteen slots.
    assert!(managed.deploy(&desired("dep_b"), &cancel).await.unwrap());
    harness.abandon_in(&core, 14).await;
    assert_eq!(backend.deployments().await.unwrap().len(), chunk_backend::MAX_DEPLOYMENTS);
    {
        let mut deployments = crate::managed::lock(&managed.deployments);
        deployments.desired = Some("dep_b".into());
        deployments.serving = Some("dep_b".into());
        deployments.loading = Some("dep_c".into());
    }

    // The tick releases them all, so the oldest deployment, which holds the player, is not retired for room.
    managed.retire().await;
    let resident: Vec<_> = backend.deployments().await.unwrap().iter().map(|id| id.as_str().to_owned()).collect();
    assert_eq!(resident, ["dep_a", "dep_b"]);
    assert_eq!(control.online_players().unwrap(), 1);
    let phases: Vec<_> = control.nodes().unwrap().iter().map(|node| node.phase).collect();
    assert!(
        !phases.is_empty() && phases.iter().all(|phase| !matches!(phase, NodePhase::Stopping | NodePhase::Stopped))
    );
    claiming.abort();
    core.stop(|| {}).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_to_retire_one_deployment_does_not_delay_the_retirement_of_another() {
    let harness = Harness::new().await;
    let core = Core::start(harness.core(), || {}).await.unwrap();
    let (gateway, lease) = (OnceLock::new(), watch::Sender::new(crate::managed::Lease::Waiting));
    let registration = crate::managed::Registration::local("env_test");
    let managed =
        Managed::new(&harness.management_config(), lease, registration, &harness.state(), &core, &gateway, None);
    let desired = |id: &str| AttachResponse {
        deployment_id: id.into(),
        release: Some(harness.valid()),
        ..AttachResponse::default()
    };
    let cancel = CancellationToken::new();
    for id in ["dep_a", "dep_b", "dep_c"] {
        assert!(managed.deploy(&desired(id), &cancel).await.unwrap());
    }
    {
        let mut deployments = crate::managed::lock(&managed.deployments);
        deployments.desired = Some("dep_c".into());
        deployments.serving = Some("dep_c".into());
        deployments.unacknowledged = None;
    }

    // An action holds dep_a's version while dep_a is asked to stop, so retiring it in the backend waits for the action.
    let backend = core.backend().unwrap();
    let call = Call {
        deployment: DeploymentId::new("dep_a").unwrap(),
        function: "wait".into(),
        arguments: serde_json::json!(null).into(),
        caller: serde_json::json!({"player": "alice"}).into(),
    };
    let id = backend.allocate_action_id().await.unwrap();
    let _action = backend.start_action(id, call).await.unwrap();
    core.control().unwrap().stop_release("dep_a").unwrap();

    // The tick returns at once, and dep_b, which is due, is released while dep_a still waits.
    tokio::time::timeout(Duration::from_secs(5), managed.retire()).await.unwrap();
    let resident = |backend: chunk_backend::Backend| async move {
        let ids = backend.deployments().await.unwrap();
        ids.iter().map(|id| id.as_str().to_owned()).collect::<Vec<_>>()
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while resident(backend.clone()).await.contains(&"dep_b".to_owned()) {
        assert!(tokio::time::Instant::now() < deadline, "dep_b was never released");
        managed.retire().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(resident(backend.clone()).await.contains(&"dep_a".to_owned()));
    core.stop(|| {}).await.unwrap();
}
