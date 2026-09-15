fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = "../../proto";
    let files = [
        "common",
        "directory",
        "edge",
        "players",
        "runtime",
        "gameplay",
        "supervision",
        "control",
        "backend",
        "session_methods",
        "hooks",
    ]
    .map(|name| format!("{root}/chunk/v1/{name}.proto"));
    println!("cargo:rerun-if-changed={root}");
    let mut prost = tonic_prost_build::Config::new();
    prost.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    for message in ["DeploymentRef", "ProcessIdentity", "ProcessHealth", "NodeList", "NodeStatus"] {
        prost.type_attribute(format!(".chunk.v1.{message}"), "#[derive(serde::Serialize, serde::Deserialize)]");
    }
    tonic_prost_build::configure().compile_with_config(prost, &files, &[root.to_owned()])?;
    Ok(())
}
