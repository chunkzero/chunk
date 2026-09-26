fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = "../../proto";
    let files = ["common", "auth", "projects", "deployments", "secrets", "domains", "environment", "edge", "logs"]
        .map(|name| format!("{root}/chunk/management/v1/{name}.proto"));
    println!("cargo:rerun-if-changed={root}/chunk/management");
    let mut prost = tonic_prost_build::Config::new();
    prost.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    let includes = [root.to_owned(), protoc_bin_vendored::include_path()?.display().to_string()];
    // Only messages: the client speaks Connect itself rather than gRPC.
    tonic_prost_build::configure()
        .build_client(false)
        .build_server(false)
        .compile_with_config(prost, &files, &includes)?;
    Ok(())
}
