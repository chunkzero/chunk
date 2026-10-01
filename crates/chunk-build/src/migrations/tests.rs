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
    let pending =
        pending_from(project.path().to_owned(), Journal::read(project.path()).unwrap(), schema(&[("name", false)]));
    assert_eq!(create(pending, "init", &Renames::new()).unwrap(), "0001_init");
    let pending = pending_from(
        project.path().to_owned(),
        Journal::read(project.path()).unwrap(),
        schema(&[("displayName", false)]),
    );
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
    fs::write(project.path().join("server/helper.ts"), "export const trim = (s: string) => s.trim();").unwrap();
    fs::write(
        &path,
        format!("import {{ trim }} from \"../helper.ts\";\n{}", source.replace("old.name", "trim(old.name)")),
    )
    .unwrap();
    rehash(project.path(), "2").unwrap();
    let error = crate::compile(project.path(), output.path()).unwrap_err();
    assert!(
        error.to_string().contains("0002_display_name.ts imports ../helper.ts; migrations may import only from #chunk"),
        "{error}"
    );
    fs::write(&path, &source).unwrap();
    rehash(project.path(), "2").unwrap();
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

    assert_eq!(migrate_to(output.path(), "Ann"), r#"[{"displayName":"Ann"}]"#);
}

fn migrate_to(output: &Path, name: &str) -> String {
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
    let mut engine = Engine::new().unwrap();
    let id = DeploymentId::new("migration-test").unwrap();
    engine.register(id.clone(), fs::read_to_string(output.join("source.mjs")).unwrap(), Limits::default()).unwrap();
    let rows = serde_json::json!({"migration": "0002_display_name", "table": "fighters", "direction": "to", "rows": [{"_id": "fighters:a", "name": name}]});
    let invocation = Invocation {
        export: "__chunk_migrate".into(),
        arguments: rows.into(),
        caller: serde_json::Value::Null.into(),
        mode: Mode::Query,
        timestamp: 0,
        seed: 0,
    };
    engine.execute(&id, invocation, Box::new(Host), &Cancellation::default()).unwrap().value
}

#[test]
fn a_held_lock_refuses_mutations_and_names_leave_room_for_finish() {
    let project = tempfile::tempdir().unwrap();
    let pending =
        pending_from(project.path().to_owned(), Journal::read(project.path()).unwrap(), schema(&[("a", false)]));
    let lock = journal::Lock::acquire(project.path()).unwrap();
    let error = create(pending, "init", &Renames::new()).unwrap_err();
    assert!(error.to_string().contains("server/migrations/.lock"), "{error}");
    assert!(finish(project.path(), "1").unwrap_err().to_string().contains(".lock"));
    drop(lock);
    assert!(!project.path().join("server/migrations/.lock").exists());

    journal::require_name(&"a".repeat(57)).unwrap();
    journal::require_name(&"a".repeat(58)).unwrap_err();
    let journal = Journal::read(project.path()).unwrap();
    journal.next_id(&journal::finish_name(&"a".repeat(57))).unwrap();
}

#[test]
fn compilation_uses_the_captured_migrations_and_ignores_package_imports() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path().canonicalize().unwrap();
    let output = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.join("server/schema")).unwrap();
    write_schema(&root, "name: v.string()");
    create(pending(&root).unwrap(), "init", &Renames::new()).unwrap();
    write_schema(&root, "displayName: v.string()");
    let renames = [("fighters".into(), [("name".into(), "displayName".into())].into())].into();
    create(pending(&root).unwrap(), "display_name", &renames).unwrap();

    let captured = verified(&root, false).unwrap();
    let path = root.join("server/migrations/0002_display_name.ts");
    fs::write(&path, fs::read_to_string(&path).unwrap().replace("old.name", "old.displayName")).unwrap();
    crate::compiler::compile_journal(&root, output.path(), &captured).unwrap();
    assert_eq!(migrate_to(output.path(), "Ann"), r#"[{"displayName":"Ann"}]"#);

    // Another compilation rewriting the shared declarations can't loosen what this one is checked against.
    fs::write(&path, fs::read_to_string(&path).unwrap().replace("{ displayName: old.displayName }", "{ }")).unwrap();
    rehash(&root, "2").unwrap();
    let incomplete = verified(&root, false).unwrap();
    let shared = root.join(".chunk/generated/migrations.ts");
    let stop = std::sync::atomic::AtomicBool::new(false);
    let error = std::thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = fs::write(&shared, "export {};\n");
            }
        });
        let result = crate::compiler::compile_journal(&root, output.path(), &incomplete);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        result.unwrap_err()
    });
    assert!(error.to_string().contains("displayName"), "{error}");
    assert!(
        fs::read_dir(root.join(".chunk")).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("compile-"))
    );

    fs::create_dir_all(root.join("server/migrations/helpers")).unwrap();
    fs::write(root.join("server/migrations/helpers/package.json"), r##"{"imports":{"#chunk":"./x.ts"}}"##).unwrap();
    let error = check(&root).unwrap_err();
    assert!(error.to_string().contains("helpers/package.json"), "{error}");
}

#[test]
fn files_the_journal_doesnt_list_are_errors_and_squash_leftovers_are_deleted() {
    let project = tempfile::tempdir().unwrap();
    create(
        pending_from(project.path().to_owned(), Journal::read(project.path()).unwrap(), schema(&[("name", false)])),
        "init",
        &Renames::new(),
    )
    .unwrap();
    let directory = project.path().join("server/migrations");
    let conflict = directory.join("0002_other.ts");
    fs::write(&conflict, "").unwrap();
    let error = check(project.path()).unwrap_err();
    assert!(error.to_string().contains("0002_other.ts"), "{error}");
    let error = finish(project.path(), "1").unwrap_err();
    assert!(error.to_string().contains("0002_other.ts"), "{error}");
    fs::remove_file(&conflict).unwrap();

    let leftover = directory.join("0001_old.ts");
    fs::write(&leftover, "").unwrap();
    let error = check(project.path()).unwrap_err();
    assert!(error.to_string().contains("chunk migrate"), "{error}");
    assert!(rehash(project.path(), "1").is_ok());
    assert!(!leftover.exists());
    check(project.path()).unwrap();
}

#[test]
fn tables_stay_in_history_and_the_journal_stops_at_the_contract_limit() {
    let table = |field: Schema| TableSchema {
        fields: [("name".to_owned(), Field { schema: field, optional: false })].into(),
        indexes: BTreeMap::new(),
    };
    let both: DatabaseSchema =
        [("fighters".to_owned(), table(Schema::String)), ("stats".to_owned(), table(Schema::String))].into();
    let project = tempfile::tempdir().unwrap();
    let start = |journal| pending_from(project.path().to_owned(), journal, both.clone());
    create(start(Journal::read(project.path()).unwrap()), "init", &Renames::new()).unwrap();

    let stats: DatabaseSchema = [("stats".to_owned(), table(Schema::Number))].into();
    let next = pending_from(project.path().to_owned(), Journal::read(project.path()).unwrap(), stats.clone());
    create(next, "stats", &Renames::new()).unwrap();
    let journal = Journal::read(project.path()).unwrap();
    assert_eq!(journal.schema()["fighters"], table(Schema::String));
    let reintroduced: DatabaseSchema = [("fighters".to_owned(), table(Schema::Number))].into();
    assert!(require_replayed(&journal, &reintroduced).is_err());
    require_replayed(&journal, &stats).unwrap();
    let mut forgotten = journal.contract(&BTreeMap::new());
    forgotten[1].schema.remove("fighters");
    assert!(chunk_contract::validate_migrations(&forgotten).is_err());

    let mut journal = journal;
    while journal.entries.len() < 256 {
        let id = journal.next_id("more").unwrap();
        let entry =
            Entry { id, kind: MigrationKind::Baseline, finishes: None, prev: String::new(), hash: String::new() };
        journal.append(entry, both.clone(), None);
    }
    let entry = Entry {
        id: "0257_more".into(),
        kind: MigrationKind::Baseline,
        finishes: None,
        prev: String::new(),
        hash: String::new(),
    };
    assert!(journal.push(entry, both, None).unwrap_err().to_string().contains("too many migrations"));
    assert!(!directory_has(project.path(), "0257"));
}

fn directory_has(project: &Path, number: &str) -> bool {
    fs::read_dir(project.join("server/migrations/meta"))
        .unwrap()
        .any(|entry| entry.unwrap().file_name().to_string_lossy().starts_with(number))
}
