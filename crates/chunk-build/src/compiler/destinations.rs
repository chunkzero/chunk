#[cfg(test)]
mod tests {
    use crate::compile;
    use serde_json::Value;
    use std::fs;

    #[test]
    fn named_destinations_compile_without_becoming_functions_and_reject_ambiguous_keys() {
        let root = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("server/schema")).unwrap();
        fs::create_dir_all(root.path().join("apps/lobby")).unwrap();
        fs::write(root.path().join("apps/lobby/app.toml"), "").unwrap();
        fs::write(root.path().join("apps/lobby/build.gradle.kts"), "").unwrap();
        fs::write(
            root.path().join("server/schema/index.ts"),
            "import {defineSchema} from '#chunk/schema'; export default defineSchema({});",
        )
        .unwrap();
        let path = root.path().join("server/destinations.ts");
        let source = "import {defineDestination,query,v} from '#chunk'; export const lobby=defineDestination({key:'main',session_type:'lobby/default',machine_profile:'local',overflow:'reject',emptyTimeoutSeconds:120}); export const helper=()=>lobby.destination; export const route=query({args:{},returns:v.destination(),handler:helper});";
        fs::write(&path, source).unwrap();
        compile(root.path(), output.path()).unwrap();
        let bytes = fs::read(output.path().join("contract.json")).unwrap();
        let contract: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(contract["destinations"]["entries"]["shared/destinations/lobby"]["empty_timeout_seconds"], 120);
        assert_eq!(contract["functions"].as_object().unwrap().len(), 1);
        assert!(contract["functions"]["shared/destinations/route"].is_object());
        compile(root.path(), output.path()).unwrap();
        assert_eq!(bytes, fs::read(output.path().join("contract.json")).unwrap());
        fs::write(&path,format!("{source} export const duplicate=defineDestination({{key:'main',session_type:'lobby/default',machine_profile:'local'}});")).unwrap();
        assert!(compile(root.path(), output.path()).unwrap_err().to_string().contains("already declared"));
        fs::write(&path, source.replace("export const lobby=", "const lobby=") + " export default lobby;").unwrap();
        assert!(compile(root.path(), output.path()).unwrap_err().to_string().contains("named exports"));
        fs::write(&path, source).unwrap();
        fs::rename(&path, root.path().join("server/other.ts")).unwrap();
        assert!(compile(root.path(), output.path()).unwrap_err().to_string().contains("server/destinations"));
    }
}
