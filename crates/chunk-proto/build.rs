fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = "../../proto";
    let files = ["common", "gameplay", "supervision", "control", "backend", "session_methods", "hooks", "commands"]
        .map(|name| format!("{root}/chunk/v1/{name}.proto"))
        .into_iter()
        .chain(["core", "gateway", "jvm", "operator"].map(|name| format!("{root}/chunk/sync/v1/{name}.proto")))
        .collect::<Vec<_>>();
    println!("cargo:rerun-if-changed={root}");
    let mut prost = tonic_prost_build::Config::new();
    prost.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    for message in ["DeploymentRef", "ProcessIdentity", "ProcessHealth", "NodeList", "NodeStatus"] {
        prost.type_attribute(format!(".chunk.v1.{message}"), "#[derive(serde::Serialize, serde::Deserialize)]");
    }
    tonic_prost_build::configure().compile_with_config(prost, &files, &[root.to_owned()])?;
    Ok(())
}
