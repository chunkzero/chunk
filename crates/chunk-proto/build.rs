fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = "../../proto";
    let internal = "internal";
    let files = ["core", "gateway", "jvm", "operator"]
        .map(|name| format!("{root}/chunk/sync/v1/{name}.proto"))
        .into_iter()
        .chain([format!("{internal}/chunk/control/v1/state.proto")])
        .collect::<Vec<_>>();
    println!("cargo:rerun-if-changed={root}");
    println!("cargo:rerun-if-changed={internal}");
    let mut prost = tonic_prost_build::Config::new();
    prost.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    // Query results fan out to many streams, which share each value rather than copy it.
    prost.bytes([".chunk.sync.v1.Entry.value"]);
    prost.type_attribute(".chunk.control.v1.DeploymentRef", "#[derive(serde::Serialize, serde::Deserialize)]");
    tonic_prost_build::configure().compile_with_config(prost, &files, &[root.to_owned(), internal.to_owned()])?;
    Ok(())
}
