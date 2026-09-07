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
    ]
    .map(|name| format!("{root}/chunk/v1/{name}.proto"));
    println!("cargo:rerun-if-changed={root}");
    let mut prost = tonic_prost_build::Config::new();
    prost.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    tonic_prost_build::configure().compile_with_config(prost, &files, &[root.to_owned()])?;
    Ok(())
}
