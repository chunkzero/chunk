use std::fs;

use chunk_contract::Deployment;
use chunk_js::DeploymentId;
use chunk_store::{Commit, DocumentKey, Operation, SqliteStore, Storage, Write};
use serde_json::{Value, json};

use crate::{Backend, Call};

#[tokio::test]
async fn compiled_match_and_profile_updates_are_atomic_and_preserve_newer_fields() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("server/schema")).unwrap();
    fs::write(
        root.path().join("server/schema/index.ts"),
        r"
import {defineSchema,defineTable,v} from '#chunk/schema';
export default defineSchema({
 profiles:defineTable({player:v.player(),wins:v.integer(),note:v.optional(v.string())}).index('by_player',['player']),
 matches:defineTable({player:v.player()}).index('by_player',['player'])
});",
    )
    .unwrap();
    fs::write(root.path().join("server/matches.ts"), r"
import {query,mutation,v,unset} from '#chunk';
import schema from './schema/index.ts';
export const record=mutation({args:{player:v.player()},returns:v.id('matches'),handler:({db},a)=>{
 const id=db.insert('matches',{player:a.player});
 const profile=db.query('profiles').withIndex('by_player',q=>q.eq('player',a.player)).unique();
 if(profile) db.patch(profile._id,{wins:profile.wins+1,note:unset});
 else db.insert('profiles',{player:a.player,wins:1});
 return id;
}});
export const duplicate=mutation({args:{player:v.player()},returns:v.id('profiles'),handler:({db},a)=>db.insert('profiles',{player:a.player,wins:0})});
export const profile=query({args:{player:v.player()},returns:v.union(v.null(),v.document('profiles',schema.tables.profiles.fields)),handler:({db},a)=>db.query('profiles').withIndex('by_player',q=>q.eq('player',a.player)).first()});
export const count=query({args:{player:v.player()},returns:v.integer(),handler:({db},a)=>db.query('matches').withIndex('by_player',q=>q.eq('player',a.player)).collect(100).length});
").unwrap();
    let output = root.path().join("compiled");
    let project = root.path().to_owned();
    let destination = output.clone();
    tokio::task::spawn_blocking(move || chunk_build::compile(&project, &destination)).await.unwrap().unwrap();
    let mut metadata: Value = serde_json::from_slice(&fs::read(output.join("contract.json")).unwrap()).unwrap();
    metadata["id"] = json!("old");
    metadata["source"] = json!(fs::read_to_string(output.join("source.mjs")).unwrap());
    let old: Deployment = serde_json::from_value(metadata).unwrap();
    let mut new = old.clone();
    new.id = "new".into();
    new.tables
        .get_mut("profiles")
        .unwrap()
        .fields
        .insert("bonus".into(), serde_json::from_value(json!({"schema":{"type":"string"},"optional":true})).unwrap());
    let path = root.path().join("data.db");
    let mut store = SqliteStore::open(&path, "local").unwrap();
    store.apply_schema(&new.tables).unwrap();
    let key = DocumentKey::new("profiles", "profiles:seed").unwrap();
    let revision = store.snapshot().unwrap().revision;
    store
        .commit(Commit {
            expected: revision,
            operation: Operation { id: "seed".into(), fingerprint: [0; 32] },
            writes: vec![Write {
                key: key.clone(),
                value: Some(json!({"player":"alex","wins":0,"note":"remove","bonus":"preserve"})),
            }],
            result: json!(null),
        })
        .unwrap();
    let backend = Backend::new("local".into(), Box::new(store)).unwrap();
    backend.deploy(old).await.unwrap();
    backend.deploy(new).await.unwrap();
    let call = |function: &str| Call {
        deployment: DeploymentId::new("old").unwrap(),
        function: format!("shared/matches/{function}"),
        arguments: json!({"player":"alex"}).into(),
        caller: Value::Null.into(),
    };
    let first = backend.mutate("one".into(), call("record")).await.unwrap();
    assert!(first.json.starts_with("\"matches:"));
    assert_eq!(backend.mutate("one".into(), call("record")).await.unwrap().json, first.json);
    let profile: Value = serde_json::from_str(&backend.query(call("profile")).await.unwrap().json).unwrap();
    assert_eq!(profile, json!({"_id":"profiles:seed","player":"alex","wins":1}));
    backend.mutate("duplicate".into(), call("duplicate")).await.unwrap();
    assert!(backend.mutate("failed".into(), call("record")).await.is_err());
    assert_eq!(&*backend.query(call("count")).await.unwrap().json, "1");
    drop(backend);
    let mut store = SqliteStore::open(&path, "local").unwrap();
    let stored = store.snapshot().unwrap().get(&key).unwrap().unwrap();
    assert_eq!(stored.value, json!({"player":"alex","wins":1,"bonus":"preserve"}));
}
