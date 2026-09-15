use super::*;
use serde_json::json;

fn project() -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::create_dir_all(root.join("server/schema")).unwrap();
    fs::create_dir_all(root.join("apps/games/arena/server")).unwrap();
    fs::create_dir_all(root.join("apps/lobby")).unwrap();
    fs::write(root.join("apps/lobby/build.gradle.kts"), "").unwrap();
    fs::write(root.join("apps/lobby/app.ts"), "import {defineApp} from '#chunk'; export default defineApp({id:'lobby',runtime:{machineProfile:'small',maxPlayers:16},destinations:{main:{implementation:'default',key:'lobby'}}});").unwrap();
    fs::write(
        root.join("server/schema/index.ts"),
        "import {defineSchema} from '#chunk/schema'; export default defineSchema({});",
    )
    .unwrap();
    fs::write(root.join("apps/games/arena/build.gradle.kts"), "error(\"never run\")").unwrap();
    fs::write(root.join("apps/scope.ts"), "import {defineScope,createHook} from '#chunk'; import {apps} from '#chunk/apps'; export default defineScope({hooks:{route:createHook('player.route',()=>apps.arena.destinations.standard)}});").unwrap();
    fs::write(root.join("apps/games/scope.ts"), "import {defineScope,createHook} from '#chunk'; export default defineScope({hooks:{gate:createHook('player.login',()=>({allow:true}))}});").unwrap();
    fs::write(
        root.join("apps/games/arena/server/behavior.ts"),
        "import {command} from '#chunk'; export const leave=command('leave',{handler:()=>{}});",
    )
    .unwrap();
    fs::write(
        root.join("apps/games/arena/app.ts"),
        r#"import {defineApp,createHook,v} from '#chunk';
import {leave} from './server/behavior.ts';
export default defineApp({
 id:'arena', runtime:{machineProfile:'small',maxPlayers:16},
 implementations:{default:{config:v.object({label:v.string()})}},
 destinations:{
  standard:{implementation:'default',key:'public-arena',config:{label:'Standard'}},
  large:{implementation:'default',key:'large-arena',machineProfile:'large',maxPlayers:32,config:{label:'Large'}},
 },
 hooks:{entered:createHook('domain.enter',()=>{})},commands:{leave}
});"#,
    )
    .unwrap();
    project
}

#[test]
fn authored_scopes_compile_with_fresh_refs_and_distinct_creation_configs() {
    let project = project();
    let output = tempfile::tempdir().unwrap();
    compile(project.path(), output.path()).unwrap();
    let contract: BackendMetadata =
        serde_json::from_slice(&fs::read(output.path().join("contract.json")).unwrap()).unwrap();
    let domains = contract.domains.unwrap();
    assert_eq!(domains.apps["arena"], "games/arena");
    assert_eq!(domains.scopes["games/arena"].parent.as_deref(), Some("games"));
    assert_eq!(domains.hooks["scopes/hooks/route"].domain, "");
    assert_eq!(domains.hooks["scopes/games/hooks/gate"].domain, "games");
    assert_eq!(domains.hooks["apps/arena/app/hooks/entered"].domain, "games/arena");
    assert_eq!(domains.commands["apps/arena/app/commands/leave"].domain, "games/arena");
    let configurations = contract.session_configurations.unwrap();
    assert_eq!(configurations.configurations.len(), 1);
    let destinations = contract.destinations.unwrap();
    assert_eq!(
        destinations.entries["apps/lobby/destinations/main"].creation.as_ref().unwrap().configuration,
        json!({})
    );
    let standard = &destinations.entries["apps/arena/destinations/standard"];
    let large = &destinations.entries["apps/arena/destinations/large"];
    assert_eq!(standard.destination.session_type, large.destination.session_type);
    assert_eq!(standard.creation.as_ref().unwrap().capacity, 16);
    assert_eq!(standard.creation.as_ref().unwrap().configuration, json!({"label":"Standard"}));
    assert_eq!(large.creation.as_ref().unwrap().capacity, 32);
    assert_eq!(large.destination.machine_profile, "large");
    let refs = fs::read_to_string(project.path().join(".chunk/generated/apps.ts")).unwrap();
    assert!(!refs.contains("from "));
    assert!(refs.contains("public-arena"));
    assert!(!refs.contains("Standard"));
}

#[test]
fn authored_app_rejects_root_only_hooks_and_invalid_creation_values() {
    let project = project();
    let root = project.path();
    let path = root.join("apps/games/arena/app.ts");
    let valid = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        valid.replace(
            "createHook('domain.enter',()=>{})",
            "createHook('player.route',()=>({key:'k',session_type:'arena/default',machine_profile:'small'}))",
        ),
    )
    .unwrap();
    let output = tempfile::tempdir().unwrap();
    assert!(compile(root, output.path()).unwrap_err().to_string().contains("root"));
    fs::write(&path, valid.replace("label:'Standard'", "label:123")).unwrap();
    assert!(compile(root, output.path()).unwrap_err().to_string().contains("number"));
}
