use super::*;
use chunk_contract::{Field, Schema, TableSchema};
use std::fs;

fn schema(fields: &[(&str, bool)]) -> DatabaseSchema {
    let fields = fields
        .iter()
        .map(|(name, optional)| ((*name).to_owned(), Field { schema: Schema::String, optional: *optional }))
        .collect();
    [("fighters".to_owned(), TableSchema { fields, indexes: BTreeMap::new() })].into()
}

fn write_schema(project: &Path, fields: &str) {
    fs::write(
        project.join("server/schema/index.ts"),
        format!("import {{defineSchema,defineTable,v}} from '#chunk/schema'; export default defineSchema({{fighters: defineTable({{{fields}}})}});"),
    )
    .unwrap();
}

#[test]
fn renames_write_mapped_transforms_and_edits_fail_their_hash() {
    let project = tempfile::tempdir().unwrap();
    let pending = pending_from(Journal::read(project.path()).unwrap(), schema(&[("name", false)]));
    assert_eq!(create(pending, "init", &Renames::new()).unwrap(), "0001_init");
    let pending = pending_from(Journal::read(project.path()).unwrap(), schema(&[("displayName", false)]));
    let renames = [("fighters".into(), [("name".into(), "displayName".into())].into())].into();
    assert_eq!(create(pending, "display_name", &renames).unwrap(), "0002_display_name");
    let path = project.path().join("server/migrations/0002_display_name.ts");
    let source = fs::read_to_string(&path).unwrap();
    assert!(source.contains("to: (old) => ({ displayName: old.name })"), "{source}");
    assert!(source.contains("back: (row) => ({ name: row.displayName })"), "{source}");
    assert!(project.path().join("server/migrations/meta/0002.snapshot.json").is_file());
    check(project.path()).unwrap();

    let journal = Journal::read(project.path()).unwrap();
    require_replayed(&journal, &schema(&[("displayName", false), ("title", true)])).unwrap();
    let error = require_replayed(&journal, &schema(&[("displayName", false), ("rank", false)])).unwrap_err();
    assert!(error.to_string().contains("chunk migrate new"), "{error}");

    fs::write(&path, format!("{source}// edited\n")).unwrap();
    let error = check(project.path()).unwrap_err();
    assert!(error.to_string().contains("no longer matches its recorded hash"), "{error}");
    rehash(project.path(), "2").unwrap();
    check(project.path()).unwrap();
    assert_eq!(finish(project.path(), "2").unwrap(), "0003_finish_display_name");
    assert_eq!(squash(project.path()).unwrap(), "0003_baseline");
    assert!(!path.exists());
    assert_eq!(Journal::read(project.path()).unwrap().schema(), schema(&[("displayName", false)]));
}

#[test]
fn migrations_are_typed_from_snapshots_and_callable_from_the_bundle() {
    use chunk_js::{Cancellation, DeploymentId, Engine, Invocation, Key, Limits, Mode, ReadHost};
    struct Host;
    impl ReadHost for Host {
        fn get(&mut self, _: &Key) -> Result<Option<serde_json::Value>, String> {
            Err("unexpected read".into())
        }
        fn scan(
            &mut self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<Vec<(String, serde_json::Value)>, String> {
            Err("unexpected scan".into())
        }
    }
    let project = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join("server/schema")).unwrap();
    write_schema(project.path(), "name: v.string()");
    create(pending(project.path()).unwrap(), "init", &Renames::new()).unwrap();
    write_schema(project.path(), "displayName: v.string()");
    let error = crate::compile(project.path(), output.path()).unwrap_err();
    assert!(error.to_string().contains("chunk migrate new"), "{error}");

    let renames = [("fighters".into(), [("name".into(), "displayName".into())].into())].into();
    create(pending(project.path()).unwrap(), "display_name", &renames).unwrap();
    let path = project.path().join("server/migrations/0002_display_name.ts");
    let source = fs::read_to_string(&path).unwrap();
    fs::write(&path, source.replace("old.name", "old.displayName")).unwrap();
    rehash(project.path(), "2").unwrap();
    let error = crate::compile(project.path(), output.path()).unwrap_err();
    assert!(error.to_string().contains("0002_display_name.ts"), "{error}");
    fs::write(&path, &source).unwrap();
    rehash(project.path(), "2").unwrap();
    crate::compile(project.path(), output.path()).unwrap();

    let contract: crate::BackendMetadata =
        serde_json::from_slice(&fs::read(output.path().join("contract.json")).unwrap()).unwrap();
    let migrations = &contract.contracts.migrations;
    assert_eq!(migrations.iter().map(|m| m.kind).collect::<Vec<_>>(), [MigrationKind::Baseline, MigrationKind::Expand]);
    let table = &migrations[1].tables["fighters"];
    assert_eq!(
        (table.added.as_slice(), table.removed.as_slice(), table.back),
        (&["displayName".to_owned()][..], &["name".to_owned()][..], true)
    );

    let mut engine = Engine::new().unwrap();
    let id = DeploymentId::new("migration-test").unwrap();
    engine
        .register(id.clone(), fs::read_to_string(output.path().join("source.mjs")).unwrap(), Limits::default())
        .unwrap();
    let rows = serde_json::json!({"migration": "0002_display_name", "table": "fighters", "direction": "to", "rows": [{"_id": "fighters:a", "name": "Ann"}]});
    let invocation = Invocation {
        export: "__chunk_migrate".into(),
        arguments: rows.into(),
        caller: serde_json::Value::Null.into(),
        mode: Mode::Query,
        timestamp: 0,
        seed: 0,
    };
    let result = engine.execute(&id, invocation, Box::new(Host), &Cancellation::default()).unwrap();
    assert_eq!(result.value, r#"[{"displayName":"Ann"}]"#);
}
