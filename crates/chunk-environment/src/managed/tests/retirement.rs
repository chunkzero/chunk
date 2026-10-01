//! Retiring a draining release whose sessions have players and whose backend version holds a scheduled job.

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

/// A deployment whose version holds a job once `hold` runs.
fn holding(id: &str) -> chunk_contract::Deployment {
    let function = |kind, visibility, arguments: serde_json::Value, result: serde_json::Value| {
        serde_json::from_value(serde_json::json!({
            "kind": kind, "visibility": visibility, "export": "", "arguments": arguments, "result": result
        }))
        .unwrap()
    };
    let at = serde_json::json!({"type": "object", "fields": {"at": {"schema": {"type": "integer"}}}});
    let mut functions: BTreeMap<String, chunk_contract::Function> = BTreeMap::from([
        ("hold".into(), function("mutation", "public", at, serde_json::json!({"type": "string"}))),
        (
            "flow".into(),
            function("action", "internal", serde_json::json!({"type": "null"}), serde_json::json!({"type": "null"})),
        ),
    ]);
    for (name, function) in &mut functions {
        function.export.clone_from(name);
    }
    chunk_contract::Deployment {
        contracts: chunk_contract::Contracts::default(),
        contract_version: 2,
        runtime_profile: chunk_contract::RuntimeProfile::TransactionalV1,
        id: id.into(),
        source: "export function hold(ctx, args) { return ctx.scheduler.runAt(args.at, 'flow', null); }\n\
                 export async function flow() { return null; }"
            .into(),
        tables: BTreeMap::new(),
        functions,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_occupied_draining_release_with_a_backend_job_is_retired_to_free_its_slot() {
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

        // A player holds a claim on dep_a's session, and dep_a's version holds a job, as do 14 versions beside it.
        let control = core.control().unwrap();
        let login = ClaimRequest {
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
        };
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
        for index in 0..14 {
            let id = format!("dep_held_{index}");
            core.deploy(holding(&id)).await.unwrap();
            backend.mutate(format!("hold-{id}"), hold(&id)).await.unwrap();
        }

        // dep_b replaces dep_a and stops it, though it has a player and a job, so the backend holds 16 versions. Then
        // dep_c needs a slot, which the oldest version, dep_a, frees.
        harness.deploy_stopping("dep_b", harness.valid());
        harness.expect(2, "dep_b", DeploymentState::InProgress).await;
        harness.expect(2, "dep_b", DeploymentState::Active).await;
        assert!(harness.serves("dep_a").await);
        harness.deploy("dep_c", harness.valid());
        harness.expect(3, "dep_c", DeploymentState::InProgress).await;
        harness.expect(3, "dep_c", DeploymentState::Active).await;
        harness.released("dep_a").await;
        assert!(harness.serves("dep_c").await);
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
