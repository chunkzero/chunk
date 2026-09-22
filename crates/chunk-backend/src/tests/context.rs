use std::{fs, time::Duration};

use chunk_contract::Deployment;
use chunk_js::DeploymentId;
use chunk_store::SqliteStore;
use serde_json::{Value, json};

use crate::{Backend, Call, Subscription};

fn call(function: &str, player: Option<&str>, arguments: Value) -> Call {
    let mut caller = json!({"session":"session-one","app":"lobby"});
    if let Some(player) = player {
        caller["player"] = player.into();
    }
    Call {
        deployment: DeploymentId::new("context-test").unwrap(),
        function: format!("shared/functions/{function}"),
        arguments: arguments.into(),
        caller: caller.into(),
    }
}

async fn next(subscription: &mut Subscription) -> crate::Result<Value> {
    let update = tokio::time::timeout(Duration::from_secs(5), subscription.next()).await.unwrap()?;
    Ok(serde_json::from_str(&update.json).unwrap())
}

#[tokio::test]
async fn compiled_context_tracks_rank_changes_and_rolls_back_rejected_mutations() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("server/schema")).unwrap();
    fs::write(root.path().join("server/schema/index.ts"), include_str!("context/schema.ts")).unwrap();
    fs::write(root.path().join("server/functions.ts"), include_str!("context/functions.ts")).unwrap();
    let project = root.path().to_owned();
    let output = root.path().join("compiled");
    let destination = output.clone();
    tokio::task::spawn_blocking(move || chunk_build::compile(&project, &destination)).await.unwrap().unwrap();
    let contract = output.join("contract.json");
    let mut metadata: Value = serde_json::from_slice(&fs::read(&contract).unwrap()).unwrap();
    assert_eq!(metadata["functions"]["shared/functions/profile"]["arguments"], json!({"type":"object","fields":{}}));
    assert_eq!(metadata["functions"]["shared/functions/internalProfile"]["visibility"], "internal");
    for (directory, target) in [
        ("java", chunk_build::GenerationTarget::Java { package: "example.context" }),
        ("kotlin", chunk_build::GenerationTarget::Kotlin { package: "example.context" }),
    ] {
        chunk_build::generate(&contract, &root.path().join(directory), target).unwrap();
    }
    metadata["id"] = json!("context-test");
    metadata["source"] = json!(fs::read_to_string(output.join("source.mjs")).unwrap());
    let deployment: Deployment = serde_json::from_value(metadata).unwrap();
    let store = SqliteStore::open(root.path().join("data.db"), "local").unwrap();
    let backend = Backend::new("local".into(), Box::new(store)).unwrap();
    backend.deploy(deployment).await.unwrap();
    for (player, rank) in [("alice", "admin"), ("bob", "member")] {
        backend
            .mutate(format!("seed-{player}"), call("seed", None, json!({"player":player,"rank":rank})))
            .await
            .unwrap();
    }

    assert!(matches!(
        backend.query(call("internalProfile", Some("alice"), json!({}))).await,
        Err(crate::Error::Unknown)
    ));
    let mut alice = backend.subscribe(call("profile", Some("alice"), json!({}))).await.unwrap();
    let mut admin = backend.subscribe(call("admin", Some("alice"), json!({}))).await.unwrap();
    assert_eq!(next(&mut alice).await.unwrap(), json!({"id":"alice","rank":"admin","visits":0}));
    assert_eq!(next(&mut admin).await.unwrap(), "alice");
    let bob: Value =
        serde_json::from_str(&backend.query(call("profile", Some("bob"), json!({}))).await.unwrap().json).unwrap();
    assert_eq!(bob, json!({"id":"bob","rank":"member","visits":0}));
    assert!(
        backend
            .query(call("profile", None, json!({})))
            .await
            .unwrap_err()
            .to_string()
            .contains("Player context required")
    );
    assert!(backend.query(call("profile", Some("bob"), json!({"player":"alice"}))).await.is_err());

    for _ in 0..2 {
        assert_eq!(
            &*backend.mutate("visit-alice".into(), call("visit", Some("alice"), json!({}))).await.unwrap().json,
            "1"
        );
    }
    assert_eq!(next(&mut alice).await.unwrap()["visits"], 1);
    assert!(
        backend
            .mutate("visit-bob".into(), call("visit", Some("bob"), json!({})))
            .await
            .unwrap_err()
            .to_string()
            .contains("Admin required")
    );
    let bob: Value =
        serde_json::from_str(&backend.query(call("profile", Some("bob"), json!({}))).await.unwrap().json).unwrap();
    assert_eq!(bob["visits"], 0);

    backend.mutate("demote".into(), call("changeRank", None, json!({"player":"alice","rank":"member"}))).await.unwrap();
    assert_eq!(next(&mut alice).await.unwrap(), json!({"id":"alice","rank":"member","visits":1}));
    assert!(next(&mut admin).await.unwrap_err().to_string().contains("Admin required"));
    backend.mutate("promote".into(), call("changeRank", None, json!({"player":"alice","rank":"admin"}))).await.unwrap();
    assert_eq!(next(&mut admin).await.unwrap(), "alice");
}
