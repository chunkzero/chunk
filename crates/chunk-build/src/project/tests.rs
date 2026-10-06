use super::*;
use serde_json::json;

fn discover_apps(root: &Path) -> io::Result<Vec<AppMetadata>> {
    discover(root).map(|inventory| inventory.apps)
}

const LOCAL: &str = r#"
[local]
environment = "local"
machine_profile = "small"
capacity = 16
max_processes = 4

[local.profiles.small]
memory_mib = 512
max_sessions = 2

[local.profiles.large]
memory_mib = 1024
max_sessions = 4
"#;

fn app(root: &Path, id: &str, manifest: &str) {
    let directory = root.join("apps").join(id);
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("app.toml"), manifest).unwrap();
    fs::write(directory.join("build.gradle.kts"), "error(\"inspection must not run Gradle\")").unwrap();
}

#[test]
fn inspection_resolves_sorted_apps_without_building_or_repeating_inventory() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(root.join("chunk.toml"), LOCAL).unwrap();
    app(root, "lobby", "");
    app(root, "arena", "[runtime]\nmachine_profile = 'large'\ncapacity = 32");
    app(&root.join("apps/lobby"), "nested", "");
    fs::create_dir_all(root.join("apps/ignored/server")).unwrap();
    fs::write(root.join("apps/ignored/server/broken.ts"), "invalid TypeScript").unwrap();

    let metadata = inspect(root).unwrap();
    let encoded = serde_json::to_value(metadata).unwrap();
    assert_eq!(
        encoded["apps"],
        json!([
            {"id": "arena", "directory": "apps/arena", "gradle_project": ":apps:arena",
             "runtime": {"machine_profile": "large", "capacity": 32}},
            {"id": "lobby", "directory": "apps/lobby", "gradle_project": ":apps:lobby",
             "runtime": {"machine_profile": "small", "capacity": 16}}
        ])
    );
    assert_eq!(encoded["version"], 1);
    assert_eq!(encoded["local"]["profiles"]["large"]["memory_mib"], 1024);
    assert!(!root.join(".chunk").exists());

    fs::write(root.join("chunk.toml"), "").unwrap();
    fs::write(root.join("apps/arena/app.toml"), "[runtime]\ncapacity = 32").unwrap();
    let metadata = inspect(root).unwrap();
    assert!(metadata.local.is_none());
    assert_eq!(metadata.apps[0].runtime.capacity, Some(32));
    assert_eq!(metadata.apps[1].runtime.capacity, None);
}

#[test]
fn manifest_errors_identify_file_and_reject_unimplemented_fields() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let error = inspect(root).unwrap_err().to_string();
    assert!(error.contains("chunk.toml"), "{error}");
    fs::write(root.join("chunk.toml"), "").unwrap();
    app(root, "lobby", "");
    for (filename, content, expected) in [
        ("chunk.toml", "[local", "TOML parse error at line 1"),
        ("chunk.toml", "apps = ['lobby']", "unknown field `apps`"),
        ("apps/lobby/app.toml", "domain = 'games'", "unknown field `domain`"),
        ("apps/lobby/app.toml", "name = 'lobby'", "unknown field `name`"),
        ("apps/lobby/app.toml", "[runtime]\njava = 25", "unknown field `java`"),
    ] {
        fs::write(root.join(filename), content).unwrap();
        let error = inspect(root).unwrap_err().to_string();
        assert!(error.contains(filename) && error.contains(expected), "{error}");
        fs::write(root.join(filename), "").unwrap();
    }
    fs::remove_file(root.join("apps/lobby/build.gradle.kts")).unwrap();
    let error = inspect(root).unwrap_err().to_string();
    assert!(error.contains("apps/lobby/build.gradle.kts"), "{error}");
}

#[test]
fn local_defaults_and_app_overrides_are_validated_against_profiles_and_limits() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    app(root, "lobby", "");
    for (valid, invalid_value, expected) in [
        ("environment = \"local\"", "environment = ''", "local.environment"),
        ("machine_profile = \"small\"", "machine_profile = 'missing'", "unknown profile"),
        ("capacity = 16", "capacity = 129", "local.capacity"),
        ("max_processes = 4", "max_processes = 0", "local.max_processes"),
        ("memory_mib = 512", "memory_mib = 1", "local.profiles.small"),
        ("max_sessions = 2", "max_sessions = 17", "local.profiles.small"),
    ] {
        fs::write(root.join("chunk.toml"), LOCAL.replace(valid, invalid_value)).unwrap();
        let error = inspect(root).unwrap_err().to_string();
        assert!(error.contains("chunk.toml") && error.contains(expected), "{error}");
    }
    fs::write(root.join("chunk.toml"), LOCAL).unwrap();
    for (manifest, expected) in [
        ("[runtime]\nmachine_profile = 'missing'", "unknown profile"),
        ("[runtime]\ncapacity = 0", "capacity"),
        ("[runtime]\ncapacity = 129", "capacity"),
        ("[sessions.default]\nmachine_profile = 'missing'", "unknown profile"),
        ("[sessions.default]\ncapacity = 0", "capacity"),
        ("[sessions.invalid-name]\ncapacity = 8", "session type IDs"),
    ] {
        fs::write(root.join("apps/lobby/app.toml"), manifest).unwrap();
        let error = inspect(root).unwrap_err().to_string();
        assert!(error.contains("apps/lobby/app.toml") && error.contains(expected), "{error}");
    }
    fs::write(root.join("chunk.toml"), "").unwrap();
    fs::write(root.join("apps/lobby/app.toml"), "[runtime]\nmachine_profile = 'small'").unwrap();
    let error = inspect(root).unwrap_err().to_string();
    assert!(error.contains("requires profiles in chunk.toml"), "{error}");
}

#[test]
fn app_ids_are_validated_before_becoming_wire_or_gradle_names() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    assert!(discover_apps(root).unwrap().is_empty());
    app(root, "invalid-name", "");
    let error = discover_apps(root).unwrap_err().to_string();
    assert!(error.contains("invalid-name/app.toml") && error.contains("ASCII identifier"), "{error}");
    fs::rename(root.join("apps/invalid-name"), root.join("apps/lobby")).unwrap();
    app(root, "Lobby", "");
    // Case-insensitive filesystems refer to the same directory.
    if fs::read_dir(root.join("apps")).unwrap().count() == 2 {
        let error = discover_apps(root).unwrap_err().to_string();
        assert!(error.contains("app.toml") && error.contains("differ only by case"), "{error}");
    }
}

#[cfg(unix)]
#[test]
fn discovery_rejects_symlinked_app_inputs() {
    use std::os::unix::fs::symlink;

    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    app(root, "lobby", "");
    for name in ["app.toml", "build.gradle.kts"] {
        let path = root.join("apps/lobby").join(name);
        let original = path.with_extension("original");
        fs::rename(&path, &original).unwrap();
        symlink(&original, &path).unwrap();
        let error = discover_apps(root).unwrap_err().to_string();
        assert!(error.contains(name) && error.contains("symlink"), "{error}");
        fs::remove_file(&path).unwrap();
        fs::rename(&original, &path).unwrap();
    }
    symlink(root.join("apps/lobby"), root.join("apps/linked")).unwrap();
    let error = discover_apps(root).unwrap_err().to_string();
    assert!(error.contains("apps/linked") && error.contains("symlink"), "{error}");
}

#[test]
fn scopes_come_from_authored_directories_with_implicit_root_and_ancestors() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    app(root, "lobby", "");
    fs::create_dir_all(root.join("apps/games/duels")).unwrap();
    fs::write(root.join("apps/games/duels/scope.ts"), "export default defineScope({});").unwrap();
    let inventory = discover(root).unwrap();
    assert_eq!(inventory.apps[0].domain, "");
    assert_eq!(inventory.scopes[""].parent, None);
    assert_eq!(inventory.scopes["games/duels"].parent.as_deref(), Some("games"));
    fs::create_dir_all(root.join("server/domains")).unwrap();
    let error = discover(root).err().unwrap().to_string();
    assert!(error.contains("server/domains is no longer supported"), "{error}");
}

#[test]
fn app_ts_discovery_uses_stable_ids_and_never_loads_imported_behavior() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(root.join("chunk.toml"), LOCAL).unwrap();
    let directory = root.join("apps/games/renamable");
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("build.gradle.kts"), "error(\"never run\")").unwrap();
    fs::write(directory.join("app.ts"), "import {defineApp} from '#chunk'; import {schema,entered} from './missing-generated.ts'; throw new Error('must not execute'); export default defineApp({id:'arena',runtime:{maxPlayers:24},implementations:{default:{config:schema}},hooks:{entered}});").unwrap();
    let metadata = inspect(root).unwrap();
    assert_eq!(metadata.apps[0].id, "arena");
    assert_eq!(metadata.apps[0].gradle_project, ":apps:games:renamable");
    assert_eq!(metadata.apps[0].domain, "games/renamable");
    assert_eq!(metadata.apps[0].runtime.capacity, Some(24));
    assert_eq!(metadata.apps[0].runtime.machine_profile.as_deref(), Some("small"));
    assert!(!root.join(".chunk").exists());
    let original = fs::read_to_string(directory.join("app.ts")).unwrap();
    for (from, to, diagnostic) in [
        ("id:'arena'", "id:computedId", "literal strings"),
        ("maxPlayers:24", "maxPlayers:12*2", "literal integer"),
        ("hooks:{entered}", "hooks:{...imported}", "spreads"),
    ] {
        fs::write(directory.join("app.ts"), original.replace(from, to)).unwrap();
        let error = inspect(root).unwrap_err().to_string();
        assert!(error.contains("app.ts") && error.contains(diagnostic), "{error}");
    }
}

#[test]
fn asset_declarations_resolve_sources_inside_assets_and_order_packs_by_scope() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let write = |path: &str, contents: &str| {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    };
    write("chunk.toml", "");
    write("apps/scope.ts", "export default defineScope({packs:{base:{source:'packs/base'}}});");
    write("apps/games/scope.ts", "export default defineScope({packs:{games:{source:'games.zip',prompt:'Games'}}});");
    write("apps/games/arena/build.gradle.kts", "");
    let app = "export default defineApp({id:'arena',worlds:{koth:{source:'worlds/koth.polar'},lobby:{source:'lobby.polar'}},packs:{ui:{source:'ui',required:true},hud:{source:'hud'}}});";
    write("apps/games/arena/app.ts", app);
    for path in [
        "assets/packs/base/pack.mcmeta",
        "apps/games/arena/assets/ui/pack.mcmeta",
        "apps/games/arena/assets/hud/pack.mcmeta",
    ] {
        write(path, "{}");
    }
    write("assets/games.zip", "");
    write("apps/games/arena/assets/lobby.polar", "");
    write("apps/games/arena/assets/worlds/koth.polar", "");
    fs::create_dir_all(root.join("apps/games/arena/assets/save.polar")).unwrap();
    let metadata = serde_json::to_value(inspect(root).unwrap()).unwrap();
    assert_eq!(
        metadata["apps"][0]["worlds"],
        json!({
            "koth": {"source": "apps/games/arena/assets/worlds/koth.polar"},
            "lobby": {"source": "apps/games/arena/assets/lobby.polar"}
        })
    );
    let contract = assets::contract(&inspect_inventory(root).unwrap());
    assert_eq!(contract.app_packs["arena"], ["base", "games", "ui", "hud"]);
    assert!(contract.packs["ui"].required && contract.packs["games"].prompt.as_deref() == Some("Games"));
    contract.validate().unwrap();

    for (from, to, expected) in [
        ("source:'lobby.polar'", "source:'../lobby.polar'", "relative path inside apps/games/arena/assets/"),
        ("source:'lobby.polar'", "source:'missing.polar'", "missing.polar"),
        (
            "source:'lobby.polar'",
            "source:'worlds'",
            "world lobby must be a .polar file; Chunk no longer converts Anvil",
        ),
        ("source:'lobby.polar'", "source:'save.polar'", "world lobby must be a .polar file"),
        (
            "source:'lobby.polar'",
            "source:'lobby.polar',chunks:{from:[0,0],to:[1,1]}",
            "unsupported option of world lobby",
        ),
        ("source:'ui'", "source:'worlds/koth.polar'", "pack.mcmeta or a .zip"),
        ("ui:{", "base:{", "pack base is declared more than once"),
    ] {
        write("apps/games/arena/app.ts", &app.replace(from, to));
        let error = inspect(root).unwrap_err().to_string();
        assert!(error.contains(expected), "{expected}: {error}");
    }
    write("apps/games/arena/app.ts", app);
    #[cfg(unix)]
    {
        fs::remove_file(root.join("assets/games.zip")).unwrap();
        std::os::unix::fs::symlink(root.join("chunk.toml"), root.join("assets/games.zip")).unwrap();
        let error = inspect(root).unwrap_err().to_string();
        assert!(error.contains("cannot be a symlink"), "{error}");
    }
}

#[test]
fn declarations_can_be_inspected_before_their_sources_exist() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let write = |path: &str, contents: &str| {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    };
    write("chunk.toml", "");
    write("apps/lobby/build.gradle.kts", "");
    let app =
        "export default defineApp({id:'lobby',worlds:{main:{source:'main.polar'}},packs:{ui:{source:'ui.zip'}}});";
    write("apps/lobby/app.ts", app);
    assert!(inspect(root).is_err());
    let metadata = inspect_declarations(root).unwrap();
    let lobby = &metadata.apps[0];
    assert_eq!(lobby.worlds["main"].source, "apps/lobby/assets/main.polar");
    assert_eq!(lobby.packs["ui"].source, "apps/lobby/assets/ui.zip");

    write("apps/lobby/app.ts", &app.replace("main.polar", "worlds/main"));
    assert!(inspect_declarations(root).unwrap_err().to_string().contains("must be a .polar file"));

    write("apps/lobby/app.ts", &app.replace("main.polar", "../main.polar"));
    assert!(inspect_declarations(root).unwrap_err().to_string().contains("relative path inside"));
}
