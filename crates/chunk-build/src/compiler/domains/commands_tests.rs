use std::fs;

use chunk_contract::{Deployment, visible_commands};
use chunk_js::{Cancellation, DeploymentId, Engine, Invocation, Limits, Mode};
use serde_json::{Value, json};

use crate::{BackendMetadata, GenerationTarget, compile, generate};

fn project() -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    for directory in ["server/schema", "server/domains/games/duels", "server/domains/games/races", "apps/duels"] {
        fs::create_dir_all(project.path().join(directory)).unwrap();
    }
    fs::write(
        project.path().join("server/schema/index.ts"),
        "import {defineSchema} from '#chunk/schema'; export default defineSchema({});",
    )
    .unwrap();
    fs::write(project.path().join("apps/duels/app.toml"), "domain='games/duels'").unwrap();
    fs::write(project.path().join("apps/duels/build.gradle.kts"), "").unwrap();
    project
}

#[test]
fn named_commands_compile_into_scoped_contracts_and_executable_handlers() {
    let project = project();
    let root = project.path();
    let output = root.join(".chunk/build");
    fs::write(root.join("server/domains/commands.ts"), r"
import {command,commandArg,commandRoute,internalQuery,query,v} from '#chunk';
export const online=query({args:{},returns:v.integer(),handler:()=>0});
export const allowed=internalQuery({args:{},returns:v.boolean(),handler:()=>true});
export const suggest=internalQuery({args:{input:v.string(),cursor:v.integer()},returns:v.array(v.string()),handler:()=>['Alex']});
export const network=command('network', { aliases:['net'],
 permission:{path:'shared/domains/commands/allowed',kind:'query',arguments:v.object({}),result:v.boolean()},
 routes:[commandRoute(['hello'], {args:{target:commandArg.word({suggestions:{path:'shared/domains/commands/suggest',kind:'query',arguments:v.object({input:v.string(),cursor:v.integer()}),result:v.array(v.string())}})},
 handler:({player},args)=>{if(player.username !== args.target) throw new Error('wrong player or route arguments');}}),
 commandRoute(['leave'],{handler:()=>{}})]});
export function helper(){return command('ignored',{handler:()=>{}})}
").unwrap();
    for scope in ["duels", "races"] {
        fs::write(
            root.join(format!("server/domains/games/{scope}/commands.mts")),
            "import {command} from '#chunk'; export const exit=command('leave',{handler:()=>{}});",
        )
        .unwrap();
    }
    compile(root, &output).unwrap();
    let contract: BackendMetadata = serde_json::from_slice(&fs::read(output.join("contract.json")).unwrap()).unwrap();
    let domains = contract.domains.as_ref().unwrap();
    assert_eq!(domains.commands.len(), 3);
    let command = &domains.commands["shared/domains/commands/network"];
    assert_eq!(command.routes[0].literals, ["hello"]);
    assert_eq!(command.permission.as_deref(), Some("shared/domains/commands/allowed"));
    assert_eq!(contract.functions.len(), 3);
    let visible = visible_commands(&domains.commands, "games/duels", &[]).unwrap();
    assert_eq!(visible["net"], "shared/domains/commands/network");
    assert_eq!(visible["leave"], "shared/domains/games/duels/commands/exit");
    assert!(visible_commands(&domains.commands, "games/duels", &["net".into()]).is_err());
    let source = fs::read_to_string(output.join("source.mjs")).unwrap();
    let mut engine = Engine::new().unwrap();
    let id = DeploymentId::new("commands").unwrap();
    engine.register(id.clone(), source, Limits::default()).unwrap();
    let result = engine
        .execute(
            &id,
            Invocation {
                export: command.export.clone(),
                arguments: json!({"route":0,"arguments":{"target":"Alex"},"player":{"uuid":"id","username":"Alex"}})
                    .into(),
                caller: Value::Null.into(),
                mode: Mode::Query,
                timestamp: 0,
                seed: 0,
            },
            Box::new(super::super::Declarations),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(result.value, "null");
    drop(engine);
    let generated = root.join("client");
    generate(&output.join("contract.json"), &generated, GenerationTarget::TypeScript).unwrap();
    let api = fs::read_to_string(generated.join("api.ts")).unwrap();
    assert!(api.contains("shared/domains/commands/online"));
    assert!(!api.contains("shared/domains/commands/allowed"));
    assert!(!api.contains("shared/domains/commands/network"));
    let mut invalid = serde_json::to_value(&contract).unwrap();
    invalid["id"] = json!("invalid");
    invalid["source"] = json!("unused");
    invalid["domains"]["commands"]["shared/domains/commands/network"]["routes"][0]["arguments"][0]["parser"] =
        json!("minecraft:message");
    assert!(serde_json::from_value::<Deployment>(invalid).is_err());
}

#[test]
fn command_compilation_rejects_misplaced_exports_and_inherited_alias_conflicts() {
    let project = project();
    let root = project.path();
    let output = root.join(".chunk/build");
    fs::write(
        root.join("server/domains/commands.ts"),
        "import {command} from '#chunk'; export const hub=command('hub',{aliases:['home'],handler:()=>{}});",
    )
    .unwrap();
    let child = root.join("server/domains/games/duels/commands.ts");
    fs::write(&child, "import {command} from '#chunk'; export const other=command('home',{handler:()=>{}});").unwrap();
    let error = compile(root, &output).unwrap_err().to_string();
    assert!(error.contains("multiple owners"), "{error}");
    fs::remove_file(child).unwrap();
    fs::write(
        root.join("server/helper.ts"),
        "import {command} from '#chunk'; export const outside=command('outside',{handler:()=>{}});",
    )
    .unwrap();
    let error = compile(root, &output).unwrap_err().to_string();
    assert!(error.contains("server/domains/**/commands.ts"), "{error}");
    fs::remove_file(root.join("server/helper.ts")).unwrap();
    fs::write(
        root.join("server/domains/commands.ts"),
        "import {command} from '#chunk'; export default command('hub',{handler:()=>{}});",
    )
    .unwrap();
    let error = compile(root, &output).unwrap_err().to_string();
    assert!(error.contains("Command descriptors require named exports"), "{error}");
}
