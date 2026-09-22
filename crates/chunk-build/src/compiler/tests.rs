use super::*;
use serde_json::json;

#[test]
fn action_declarations_compile_and_execute_in_the_isolated_runner() {
    struct Host;
    impl chunk_js::ActionHost for Host {
        fn call(
            &self,
            _: u32,
            _: Mode,
            _: String,
            _: chunk_js::Json,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<String, String>>>> {
            Box::pin(async { Err("unexpected transaction".into()) })
        }
    }
    let project = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join("server/schema")).unwrap();
    fs::write(
        project.path().join("server/schema/index.ts"),
        "import {defineSchema} from '#chunk/schema'; export default defineSchema({});",
    )
    .unwrap();
    fs::write(project.path().join("server/tasks.ts"), "import {action,v} from '#chunk'; export const work=action({args:{},returns:v.string(),handler:async(ctx)=>{ await ctx.sleep(1); return ctx.invocationId; }});").unwrap();
    compile(project.path(), output.path()).unwrap();
    let contract: BackendMetadata =
        serde_json::from_slice(&fs::read(output.path().join("contract.json")).unwrap()).unwrap();
    let function = &contract.functions["shared/tasks/work"];
    assert_eq!(function.kind, chunk_contract::FunctionKind::Action);
    let mut engine = Engine::new().unwrap();
    let id = DeploymentId::new("action-test").unwrap();
    engine
        .register(id.clone(), fs::read_to_string(output.path().join("source.mjs")).unwrap(), Limits::default())
        .unwrap();
    let result = engine
        .execute_action(
            &id,
            chunk_js::ActionInvocation {
                id: "accepted-id".into(),
                export: function.export.clone(),
                arguments: json!({}).into(),
                caller: Value::Null.into(),
                timestamp: 0,
                seed: 0,
                deadline: std::time::Instant::now() + std::time::Duration::from_secs(1),
            },
            std::rc::Rc::new(Host),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(result.value, "\"accepted-id\"");
    assert!(result.writes.is_empty());
}

#[test]
fn scheduled_internal_actions_compile_from_a_fresh_project_without_generated_references() {
    struct Host;
    impl ReadHost for Host {
        fn schedule_id(&self, sequence: u32) -> Result<String, String> {
            Ok(format!("schedule-{sequence}"))
        }
        fn get(&mut self, _: &Key) -> Result<Option<Value>, String> {
            Err("unexpected read".into())
        }
        fn scan(&mut self, _: &str, _: Option<&str>, _: Option<&str>) -> Result<Vec<(String, Value)>, String> {
            Err("unexpected scan".into())
        }
    }
    let project = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join("server/schema")).unwrap();
    fs::write(
        project.path().join("server/schema/index.ts"),
        "import {defineSchema} from '#chunk/schema'; export default defineSchema({});",
    )
    .unwrap();
    fs::write(project.path().join("server/tasks.ts"),r"import {internalAction,mutation,v} from '#chunk';
        const args=v.object({name:v.string()});
        const reference={path:'shared/tasks/work',kind:'action' as const,arguments:args,result:v.null()};
        export const work=internalAction({args,returns:v.null(),handler:()=>null});
        export const enqueue=mutation({args:{at:v.integer()},returns:v.string(),handler:({scheduler},{at})=>scheduler.runAt(at,reference,{name:'alice'})});").unwrap();
    compile(project.path(), output.path()).unwrap();
    let contract: BackendMetadata =
        serde_json::from_slice(&fs::read(output.path().join("contract.json")).unwrap()).unwrap();
    let mut engine = Engine::new().unwrap();
    let id = DeploymentId::new("schedule-test").unwrap();
    engine
        .register(id.clone(), fs::read_to_string(output.path().join("source.mjs")).unwrap(), Limits::default())
        .unwrap();
    let result = engine
        .execute(
            &id,
            Invocation {
                export: contract.functions["shared/tasks/enqueue"].export.clone(),
                arguments: json!({"at":10}).into(),
                caller: json!(null).into(),
                mode: Mode::Mutation,
                timestamp: 1,
                seed: 1,
            },
            Box::new(Host),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(result.value, r#""schedule-0""#);
    assert!(
        matches!(&result.jobs[..],[chunk_js::ScheduleIntent::RunAt {id:Some(id),at:10,function,arguments}] if id=="schedule-0" && function=="shared/tasks/work" && arguments==&json!({"name":"alice"}))
    );
}

#[test]
fn compilation_diagnostics_identify_invalid_schema_and_deployment() {
    let project = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join("server/schema")).unwrap();
    let schema = project.path().join("server/schema/index.ts");
    fs::write(&schema, "import {defineTable,v} from '#chunk/schema'; export default defineTable({name:v.string()});")
        .unwrap();
    let error = compile(project.path(), output.path()).unwrap_err().to_string();
    assert!(
        error.contains("server/schema/index.ts must default-export a schema created with defineSchema()"),
        "{error}"
    );
    fs::write(&schema, "import {defineSchema} from '#chunk/schema'; export default defineSchema({});").unwrap();
    fs::write(
        project.path().join("server/invalid-name.ts"),
        "import {query,v} from '#chunk'; export const value=query({args:{},returns:v.null(),handler:()=>null});",
    )
    .unwrap();
    let error = compile(project.path(), output.path()).unwrap_err().to_string();
    assert!(error.contains("invalid function path"), "{error}");
    assert!(error.contains(&project.path().canonicalize().unwrap().display().to_string()), "{error}");
    assert!(!error.contains('\u{1b}'), "{error}");
}

#[test]
fn clean_typescript_compilation_produces_deterministic_executable_contracts() {
    let project = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join("server/schema")).unwrap();
    fs::create_dir_all(project.path().join("apps/duels/server")).unwrap();
    fs::write(project.path().join("apps/duels/app.toml"), "").unwrap();
    fs::write(project.path().join("apps/duels/build.gradle.kts"), "").unwrap();
    fs::write(project.path().join("server/schema/index.ts"), "import {defineSchema,defineTable,v} from '#chunk/schema'; export default defineSchema({profiles:defineTable({player:v.player()}).index('by_player',['player'])});").unwrap();
    fs::write(project.path().join("apps/duels/server/match.ts"), "import {query,internalMutation,v} from '#chunk'; export function helper(n:number){return n+1} export const score=query({args:{value:v.integer()},returns:v.integer(),handler:(_,a)=>helper(a.value)}); export const hidden=internalMutation({args:{},returns:v.null(),handler:()=>null});").unwrap();
    for directory in [".chunk/generated", "server/.chunk/build", "apps/duels/server/.chunk/sdk", "server/_generated"] {
        fs::create_dir_all(project.path().join(directory)).unwrap();
        fs::write(project.path().join(directory).join("ignored.ts"), "invalid TypeScript").unwrap();
    }
    compile(project.path(), output.path()).unwrap();
    assert_eq!(fs::read_dir(output.path()).unwrap().count(), 3);
    check_editor(project.path());
    fs::create_dir_all(project.path().join("apps/unregistered/server")).unwrap();
    fs::write(project.path().join("apps/unregistered/server/ignored.ts"), "invalid TypeScript").unwrap();
    let contract = fs::read(output.path().join("contract.json")).unwrap();
    let decoded: BackendMetadata = serde_json::from_slice(&contract).unwrap();
    assert_eq!(decoded.functions.len(), 2);
    assert_eq!(decoded.tables["profiles"].indexes["by_player"], ["player"]);
    let source = fs::read_to_string(output.path().join("source.mjs")).unwrap();
    let mut engine = Engine::new().unwrap();
    let id = DeploymentId::new("test").unwrap();
    engine.register(id.clone(), source.clone(), Limits::default()).unwrap();
    let result = engine
        .execute(
            &id,
            Invocation {
                export: decoded.functions["apps/duels/match/score"].export.clone(),
                arguments: json!({"value":2}).into(),
                caller: Value::Null.into(),
                mode: Mode::Query,
                timestamp: 0,
                seed: 0,
            },
            Box::new(Declarations),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(result.value, "3");
    drop(engine);
    let source_map = fs::read(output.path().join("source.mjs.map")).unwrap();
    fs::remove_file(project.path().join(".chunk/generated/index.ts")).unwrap();
    fs::write(project.path().join(".chunk/sdk/functions.ts"), "stale SDK").unwrap();
    compile(project.path(), output.path()).unwrap();
    assert_eq!(contract, fs::read(output.path().join("contract.json")).unwrap());
    assert_eq!(source, fs::read_to_string(output.path().join("source.mjs")).unwrap());
    assert_eq!(source_map, fs::read(output.path().join("source.mjs.map")).unwrap());
    let checkout = tempfile::tempdir().unwrap();
    for file in
        ["server/schema/index.ts", "apps/duels/server/match.ts", "apps/duels/app.toml", "apps/duels/build.gradle.kts"]
    {
        let destination = checkout.path().join(file);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::copy(project.path().join(file), destination).unwrap();
    }
    let rebuilt = checkout.path().join(".chunk/build");
    compile(checkout.path(), &rebuilt).unwrap();
    assert_eq!(contract, fs::read(rebuilt.join("contract.json")).unwrap());
    assert_eq!(source, fs::read_to_string(rebuilt.join("source.mjs")).unwrap());
    assert_eq!(source_map, fs::read(rebuilt.join("source.mjs.map")).unwrap());
    fs::write(project.path().join("server/bad.ts"), "// @ts-ignore\nimport 'node:fs'; export const value=1;").unwrap();
    assert!(compile(project.path(), output.path()).is_err());
    fs::write(project.path().join("server/bad.ts"), "export const value=Date.now();").unwrap();
    assert!(compile(project.path(), output.path()).is_err());
}

#[test]
fn generated_helpers_infer_the_live_schema_and_resolve_package_imports() {
    let project = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join("server/schema")).unwrap();
    fs::create_dir_all(project.path().join("apps/duels/server")).unwrap();
    fs::write(project.path().join("apps/duels/app.toml"), "").unwrap();
    fs::write(project.path().join("apps/duels/build.gradle.kts"), "").unwrap();
    let schema = project.path().join("server/schema/index.ts");
    let schema_source = "import {defineSchema,defineTable,v} from '#chunk/schema'; export default defineSchema({profiles:defineTable({wins:v.integer(),note:v.optional(v.string())}).index('by_wins',['wins']),matches:defineTable({score:v.integer()}).index('by_score',['score'])});";
    fs::write(&schema, schema_source).unwrap();
    fs::write(project.path().join("server/helpers.ts"), include_str!("helper-types.ts")).unwrap();
    fs::write(
        project.path().join("package.json"),
        r##"{"type":"module","imports":{"#helpers":"./server/helpers.ts"}}"##,
    )
    .unwrap();
    fs::write(project.path().join("apps/duels/server/profile.ts"), "import {query,v} from '#chunk'; import {getProfile} from '#helpers'; export const read=query({args:{id:v.id('profiles')},returns:v.integer(),handler:(ctx,{id})=>getProfile(ctx,id)?.wins??0});").unwrap();
    compile(project.path(), output.path()).unwrap();
    check_editor(project.path());
    let contract: BackendMetadata =
        serde_json::from_slice(&fs::read(output.path().join("contract.json")).unwrap()).unwrap();
    assert_eq!(contract.functions.len(), 5);
    assert_eq!(contract.functions["shared/helpers/internalWins"].visibility, chunk_contract::Visibility::Internal);

    let generated = project.path().join(".chunk/generated/index.ts");
    let timestamp = fs::metadata(&generated).unwrap().modified().unwrap();
    fs::write(&schema, schema_source.replace("wins:v.integer()", "wins:v.integer(),added:v.string()")).unwrap();
    fs::write(
        project.path().join("server/new-field.ts"),
        "import type {Doc} from '#chunk'; export const added=(doc:Doc<'profiles'>):string=>doc.added;",
    )
    .unwrap();
    let inventory = crate::project::load(project.path()).unwrap();
    let files = sources::discover(project.path(), &inventory).unwrap();
    // No generation between schema edits: TypeScript follows typeof schema.tables.
    let error = typecheck::check(&files, output.path()).unwrap_err().to_string();
    assert!(error.contains("added") && error.contains("missing"), "{error}");
    fs::write(
        project.path().join("server/helpers.ts"),
        include_str!("helper-types.ts").replace("{ wins: 1 }", "{ wins: 1, added: 'new' }"),
    )
    .unwrap();
    typecheck::check(&files, output.path()).unwrap();
    assert_eq!(fs::metadata(generated).unwrap().modified().unwrap(), timestamp);
}

#[test]
fn session_contracts_compile_from_authored_refs_before_jvm_outputs_exist() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::create_dir_all(root.join("server/schema")).unwrap();
    fs::create_dir_all(root.join("apps/duels/server")).unwrap();
    fs::write(root.join("apps/duels/app.toml"), "").unwrap();
    fs::write(root.join("apps/duels/build.gradle.kts"), "").unwrap();
    fs::write(
        root.join("server/schema/index.ts"),
        "import {defineSchema} from '#chunk/schema'; export default defineSchema({});",
    )
    .unwrap();
    fs::write(root.join("apps/duels/server/methods.ts"), "import {sessionMethod,v} from '#chunk'; export const forfeit=sessionMethod({app:'duels',session:'default',name:'forfeit',args:{player:v.player()},returns:v.boolean()});").unwrap();
    fs::write(root.join("server/consumer.ts"), "import {forfeit} from '../apps/duels/server/methods.ts'; import type {SessionMethodReference,PlayerId} from '#chunk'; const reference:SessionMethodReference<{player:PlayerId},boolean>=forfeit; export function methodName(){return reference.name;}").unwrap();
    let output = root.join(".chunk/build/backend");
    compile(root, &output).unwrap();
    let contract: BackendMetadata = serde_json::from_slice(&fs::read(output.join("contract.json")).unwrap()).unwrap();
    assert!(contract.functions.is_empty());
    let methods = contract.contracts.session_methods.unwrap();
    assert_eq!(methods.methods.len(), 1);
    assert_eq!(methods.methods[0].app, "duels");
    crate::generate(
        &output.join("contract.json"),
        &root.join(".chunk/generated/jvm"),
        crate::GenerationTarget::Java { package: "example.generated" },
    )
    .unwrap();
    let source =
        fs::read_to_string(root.join(".chunk/generated/jvm/java/example/generated/SessionMethods.java")).unwrap();
    assert!(source.contains("Boolean forfeit(SessionMethods.Duels.Default.Forfeit.Args args)"), "{source}");
    check_editor(root);
    fs::write(root.join("server/invalid.ts"), "import {forfeit} from '../apps/duels/server/methods.ts'; import type {SessionMethodReference} from '#chunk'; const reference:SessionMethodReference<{player:number},boolean>=forfeit; export function value(){return reference.name;}").unwrap();
    assert!(compile(root, &output).unwrap_err().to_string().contains("number"));
}

fn check_editor(project: &Path) {
    let result = std::process::Command::new(typecheck::executable().unwrap())
        .args(["--pretty", "false", "--project"])
        .arg(project.join("tsconfig.json"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

mod authoring;
mod destinations;
