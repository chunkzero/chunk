use std::fs;

use crate::{BackendMetadata, GenerationTarget, compile, generate};

fn project() -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::create_dir_all(root.join("server/schema")).unwrap();
    fs::create_dir_all(root.join("server/domains/games/duels")).unwrap();
    fs::create_dir_all(root.join("apps/duels")).unwrap();
    fs::write(
        root.join("server/schema/index.ts"),
        "import {defineSchema} from '#chunk/schema'; export default defineSchema({});",
    )
    .unwrap();
    fs::write(root.join("apps/duels/app.toml"), "domain = 'games/duels'").unwrap();
    fs::write(root.join("apps/duels/build.gradle.kts"), "").unwrap();
    project
}

#[test]
fn compiled_domains_pin_ancestry_bindings_and_named_hooks_separately_from_functions() {
    let project = project();
    let root = project.path();
    let output = root.join(".chunk/build");
    fs::write(
        root.join("server/domains/hooks.ts"),
        r"
import {createHook, query, v} from '#chunk';
export const checkBan = createHook('player.login', () => ({allow:true}), {order:10});
export const read = query({args:{},returns:v.integer(),handler:()=>7});
export function helper() { return 'ordinary helper'; }
export const status = createHook('server.ping', () => ({motd:'Hello',online:0,max:16}));
",
    )
    .unwrap();
    fs::write(
        root.join("server/domains/games/duels/hooks.mts"),
        r"
import {createHook} from '#chunk';
export const checkEntry = createHook('player.beforeMove', (ctx) => ({allow:ctx.sourceDomain !== 'blocked'}));
export const arrived = createHook('domain.enter', () => {}, {followPlayer:true});
",
    )
    .unwrap();
    compile(root, &output).unwrap();
    let contract: BackendMetadata = serde_json::from_slice(&fs::read(output.join("contract.json")).unwrap()).unwrap();
    let domains = contract.contracts.domains.unwrap();
    assert_eq!(domains.version, 1);
    assert_eq!(domains.scopes["games/duels"].parent.as_deref(), Some("games"));
    assert_eq!(domains.apps["duels"], "games/duels");
    assert_eq!(domains.hooks.len(), 4);
    assert_eq!(domains.hooks["shared/domains/hooks/checkBan"].order, Some(10));
    assert!(domains.hooks["shared/domains/games/duels/hooks/arrived"].follow_player);
    assert_eq!(contract.functions.len(), 1);
    assert!(contract.functions.contains_key("shared/domains/hooks/read"));
    let generated = root.join("client");
    generate(&output.join("contract.json"), &generated, GenerationTarget::TypeScript).unwrap();
    let api = fs::read_to_string(generated.join("api.ts")).unwrap();
    assert!(api.contains("shared/domains/hooks/read"));
    assert!(!api.contains("checkBan"));
    assert!(!api.contains("arrived"));
}

#[test]
fn compilation_rejects_ambiguous_and_misplaced_hook_descriptors() {
    let project = project();
    let root = project.path();
    let output = root.join(".chunk/build");
    for (source, expected) in [
        (
            "export const a=createHook('player.login',()=>({allow:true})); export const b=createHook('player.login',()=>({allow:true}));",
            "distinct explicit order",
        ),
        (
            "export const a=createHook('server.ping',()=>({motd:'A',online:0,max:1})); export const b=createHook('server.ping',()=>({motd:'B',online:0,max:1}));",
            "ambiguous single-result",
        ),
        ("export default createHook('player.login',()=>({allow:true}));", "require named exports"),
    ] {
        fs::write(root.join("server/domains/hooks.ts"), format!("import {{createHook}} from '#chunk'; {source}"))
            .unwrap();
        let error = compile(root, &output).unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
        assert!(!output.join("contract.json").exists());
    }
    fs::remove_file(root.join("server/domains/hooks.ts")).unwrap();
    fs::write(
        root.join("server/helpers.ts"),
        "import {createHook} from '#chunk'; export const outside=createHook('player.login',()=>({allow:true}));",
    )
    .unwrap();
    let error = compile(root, &output).unwrap_err().to_string();
    assert!(error.contains("server/domains/**/hooks.ts"), "{error}");
}
