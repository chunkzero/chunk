use std::fs;

use super::*;

async fn compile_command() -> Deployment {
    let root = tempfile::tempdir().unwrap();
    for directory in ["server/schema", "server/domains", "apps/lobby"] {
        fs::create_dir_all(root.path().join(directory)).unwrap();
    }
    for (file, source) in [
        ("server/schema/index.ts", include_str!("schema.ts")),
        ("server/state.ts", include_str!("state.ts")),
        ("server/domains/commands.ts", include_str!("command.ts")),
        ("apps/lobby/app.toml", "domain = ''"),
        ("apps/lobby/build.gradle.kts", ""),
    ] {
        fs::write(root.path().join(file), source).unwrap();
    }
    let project = root.path().to_owned();
    let output = root.path().join("compiled");
    let destination = output.clone();
    tokio::task::spawn_blocking(move || chunk_build::compile(&project, &destination)).await.unwrap().unwrap();
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("contract.json")).unwrap()).unwrap();
    metadata["id"] = json!("commands");
    metadata["source"] = json!(fs::read_to_string(output.join("source.mjs")).unwrap());
    serde_json::from_value(metadata).unwrap()
}

#[tokio::test]
async fn compiled_sdk_command_runs_typed_mutation_player_effect_and_session_call() {
    let mut fixture = Fixture::with_deployment(compile_command().await).await;
    let prepared = fixture.prepare("notify hello world").await;
    let (sender, mut output) = fixture.run(&prepared.invocation_id).await;
    assert!(matches!(frame(&mut output).await, command_server_frame::Frame::Accepted(_)));
    for (expected, result) in [
        (json!({"kind":"message","text":"Alice: hello world (#1)"}), None),
        (
            json!({"kind":"session_call","method":{"app":"lobby","session":"main","name":"population"},"arguments":{"expected":1}}),
            Some(json!({"ready":true,"total":1})),
        ),
    ] {
        let command_server_frame::Frame::Effect(effect) = frame(&mut output).await else {
            panic!("compiled command effect")
        };
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&effect.request_json).unwrap(), expected);
        let result = result.unwrap_or_else(|| json!({"state":"accepted","operationId":effect.operation_id}));
        sender
            .send(wire::CommandClientFrame {
                frame: Some(command_client_frame::Frame::Reply(wire::CommandEffectReply {
                    sequence: effect.sequence,
                    result_json: serde_json::to_vec(&result).unwrap(),
                    error: String::new(),
                })),
            })
            .await
            .unwrap();
    }
    let command_server_frame::Frame::Finished(result) = frame(&mut output).await else {
        panic!("compiled command completion")
    };
    assert_eq!(result.state, i32::from(wire::CommandCompletionState::Succeeded), "{}", result.error);
    assert_eq!(result.result_json, b"null");
    let stored = fixture
        .backend
        .query(Call {
            deployment: DeploymentId::new("commands").unwrap(),
            function: "shared/state/read".into(),
            arguments: json!({}).into(),
            caller: json!(null).into(),
        })
        .await
        .unwrap();
    let rows: Vec<serde_json::Value> = serde_json::from_str(&stored.json).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["text"], "hello world");
    let caller: serde_json::Value = serde_json::from_str(rows[0]["caller"].as_str().unwrap()).unwrap();
    assert_eq!(
        caller,
        json!({"kind":"command","proxyId":"proxy","player":"alice","username":"Alice","session":"session-one","app":"lobby","sessionType":"lobby/main","domain":"","scopeId":"scope-one","connectionId":"connection-one","claimOperationId":"claim-one","membershipGeneration":"1","deliveryGeneration":"1"})
    );
    fixture.close().await;
}
