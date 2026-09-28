use std::fs;

use super::*;

async fn compile_command() -> Deployment {
    let root = tempfile::tempdir().unwrap();
    for directory in ["server/schema", "server/commands", "apps/lobby"] {
        fs::create_dir_all(root.path().join(directory)).unwrap();
    }
    for (file, source) in [
        ("server/schema/index.ts", include_str!("schema.ts")),
        ("server/state.ts", include_str!("state.ts")),
        ("server/commands/notify.ts", include_str!("command.ts")),
        (
            "apps/scope.ts",
            "import {defineScope} from '#chunk'; import {notify} from '../server/commands/notify.ts'; export default defineScope({commands:{notify}});",
        ),
        ("apps/lobby/app.toml", ""),
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
    let fixture = Fixture::with_deployment(compile_command().await).await;
    let (mut action, mut effects) = fixture.start("notify hello world").await.unwrap();
    let message = effect(&mut effects).await;
    assert_eq!(request(&message), json!({"kind":"message","text":"Alice: hello world (#1)"}));
    message.accept();
    let call = effect(&mut effects).await;
    assert_eq!(
        request(&call),
        json!({"kind":"session_call","method":{"app":"lobby","session":"main","name":"population"},"arguments":{"expected":1}})
    );
    call.finish(Some(&serde_json::to_vec(&json!({"ready":true,"total":1})).unwrap()));
    assert_eq!(&*outcome(&mut action).await.unwrap(), "null");
    let read = Call {
        deployment: id(),
        function: "shared/state/read".into(),
        arguments: json!({}).into(),
        caller: json!(null).into(),
    };
    let rows: Vec<serde_json::Value> = serde_json::from_str(&fixture.backend.query(read).await.unwrap().json).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["text"], "hello world");
    let recorded: serde_json::Value = serde_json::from_str(rows[0]["caller"].as_str().unwrap()).unwrap();
    assert_eq!(recorded, json!({"kind":"gateway","player":"alice"}));
}
