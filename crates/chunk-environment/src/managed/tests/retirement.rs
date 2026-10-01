//! Retiring a stopped release whose sessions have players and whose backend version holds a scheduled job.

use super::*;
use crate::{Core, managed::Managed};
use chunk_backend::{Call, DeploymentId};
use chunk_proto::control::v1::{ClaimRequest, Identity, SessionDemand};
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
async fn a_pending_deployment_forces_out_the_oldest_when_all_sixteen_resist_release() {
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
    let control = core.control().unwrap();
    let backend = core.backend().unwrap();

    // Every version holds a job, and a player holds the oldest deployment's session, so none releases on its own.
    let mut claiming = None;
    for index in 0..chunk_backend::MAX_DEPLOYMENTS {
        let id = format!("dep_{index}");
        assert!(managed.deploy(&desired(&id), &cancel).await.unwrap());
        if index == 0 {
            claiming = Some(tokio::spawn({
                let control = control.clone();
                async move { control.claim(login()).await }
            }));
            tokio::time::timeout(Duration::from_secs(30), async {
                while control.online_players().unwrap() == 0 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .expect("the claim reserves a place");
        }
        backend.mutate(format!("hold-{id}"), hold(&id)).await.unwrap();
    }
    {
        let mut deployments = crate::managed::lock(&managed.deployments);
        deployments.desired = Some("dep_15".into());
        deployments.serving = Some("dep_15".into());
        deployments.unacknowledged = None;
        deployments.loading = Some("dep_pending".into());
    }

    // The first tick finds nothing to release and durably asks the oldest deployment to stop.
    managed.retire().await;
    assert!(control.stopping().unwrap().iter().any(|name| name == "dep_0"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while backend.deployments().await.unwrap().iter().any(|id| id.as_str() == "dep_0") {
        assert!(tokio::time::Instant::now() < deadline, "dep_0 was never released");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    crate::managed::lock(&managed.deployments).loading = None;
    assert!(managed.deploy(&desired("dep_pending"), &cancel).await.unwrap());
    assert!(backend.deployments().await.unwrap().iter().any(|id| id.as_str() == "dep_pending"));
    claiming.unwrap().abort();
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

/// Deploys `id` with the first `length` entries of a journal that renames `fighters.name` to `displayName` and
/// finishes the rename.
fn renaming(id: &str, length: usize) -> chunk_contract::Deployment {
    use chunk_contract::{DatabaseSchema, Migration, MigrationKind, MigrationTable};
    let schema = |field: &str, optional: bool| -> DatabaseSchema {
        let field = serde_json::json!({field: {"schema": {"type": "string"}, "optional": optional}});
        serde_json::from_value(serde_json::json!({"fighters": {"fields": field}})).unwrap()
    };
    let rename = MigrationTable { added: vec!["displayName".into()], removed: vec!["name".into()], back: true };
    let entry = |id: &str, kind, finishes: Option<&str>, tables, schema| Migration {
        id: id.into(),
        hash: "0".repeat(64),
        kind,
        finishes: finishes.map(Into::into),
        tables,
        schema,
    };
    let journal = [
        entry("0001_init", MigrationKind::Baseline, None, BTreeMap::new(), schema("name", false)),
        entry(
            "0002_rename",
            MigrationKind::Expand,
            None,
            [("fighters".into(), rename.clone())].into(),
            schema("displayName", true),
        ),
        entry(
            "0003_finish_rename",
            MigrationKind::Finish,
            Some("0002_rename"),
            [("fighters".into(), MigrationTable { removed: rename.removed, ..MigrationTable::default() })].into(),
            schema("displayName", true),
        ),
    ];
    chunk_contract::Deployment {
        contracts: chunk_contract::Contracts { migrations: journal[..length].to_vec(), ..Default::default() },
        contract_version: chunk_contract::CONTRACT_VERSION,
        runtime_profile: chunk_contract::RuntimeProfile::TransactionalV1,
        id: id.into(),
        source: "export {};".into(),
        tables: journal[length - 1].schema.clone(),
        functions: BTreeMap::new(),
    }
}

async fn resident_in(core: &Core) -> Vec<String> {
    let ids = core.backend().unwrap().deployments().await.unwrap();
    ids.iter().map(|id| id.as_str().to_owned()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fork_keeps_its_restored_contracts_until_its_first_deployment_installs_even_across_a_restart() {
    let harness = Harness::new().await;
    // What a fork restores: a source that finished a rename while the old shape was kept, then rolled back to the
    // release before it. The fork's control never ran any of these.
    let core = Core::start(harness.core(), || {}).await.unwrap();
    for deployment in [renaming("dep_old", 1), renaming("dep_renamed", 3), renaming("dep_rollback", 1)] {
        core.deploy(deployment).await.unwrap();
    }
    core.backend().unwrap().release(DeploymentId::new("dep_old").unwrap()).await.unwrap();
    core.stop(|| {}).await.unwrap();

    // Management deploys the rolled-back release to the fork, whose archive is slow to arrive, and core restarts
    // meanwhile.
    for _ in 0..2 {
        let core = Core::start(harness.core(), || {}).await.unwrap();
        let (gateway, lease) = (OnceLock::new(), watch::Sender::new(crate::managed::Lease::Waiting));
        let registration = crate::managed::Registration::local("env_test");
        let managed =
            Managed::new(&harness.management_config(), lease, registration, &harness.state(), &core, &gateway, None);
        {
            let mut deployments = crate::managed::lock(&managed.deployments);
            deployments.desired = Some("dep_fork".into());
            deployments.loading = Some("dep_fork".into());
        }
        for _ in 0..3 {
            managed.retire().await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert_eq!(resident_in(&core).await, ["dep_renamed", "dep_rollback"]);
        drop(managed);
        core.stop(|| {}).await.unwrap();
    }

    // Once its archive arrives, the rolled-back release still installs.
    let core = Core::start(harness.core(), || {}).await.unwrap();
    core.deploy(renaming("dep_fork", 1)).await.unwrap();
    core.stop(|| {}).await.unwrap();
}
