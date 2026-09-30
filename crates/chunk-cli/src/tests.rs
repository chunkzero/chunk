use super::*;
use clap::CommandFactory;

#[test]
fn command_structure_is_valid() {
    Cli::command().debug_assert();
    assert!(Cli::try_parse_from(["chunk", "create", "demo"]).is_ok());
    assert!(Cli::try_parse_from(["chunk", "create", "demo", "--chunk-source", "source"]).is_ok());
    assert!(Cli::try_parse_from(["chunk", "create", "demo", "--chunk-source", "source", "--language", "java"]).is_ok());
    assert!(
        Cli::try_parse_from(["chunk", "create", "demo", "--chunk-source", "source", "--language", "scala"]).is_err()
    );
    assert!(Cli::try_parse_from(["chunk", "gen"]).is_err());
    assert!(Cli::try_parse_from(["chunk", "gen", "--target", "java"]).is_ok());
    assert!(Cli::try_parse_from(["chunk", "gen", "--target", "typescript"]).is_ok());
    assert!(Cli::try_parse_from(["chunk", "gen", "--target", "kotlin"]).is_ok());
    assert!(Cli::try_parse_from(["chunk", "build", "example", "--output", "dist"]).is_ok());
    assert!(Cli::try_parse_from(["chunk", "dev", "example"]).is_ok());
    assert!(Cli::try_parse_from(["chunk", "local", "example", "--java", "/jdk/bin/java"]).is_ok());
    assert!(Cli::try_parse_from(["chunk", "dev", "--project", "project.json"]).is_err());
    assert!(Cli::try_parse_from(["chunk", "auth", "login", "--cloud", "--url", "https://example.com"]).is_err());
    assert!(Cli::try_parse_from(["chunk", "deploy", "example", "--env", "staging", "--project", "demo"]).is_ok());
    assert!(Cli::try_parse_from(["chunk", "deploy"]).is_err());
    assert!(Cli::try_parse_from(["chunk", "environments", "create", "staging", "--project", "demo"]).is_ok());
    assert!(Cli::try_parse_from(["chunk", "logs", "--env", "staging", "--follow", "--app", "lobby"]).is_ok());
    let host = "5a9e4aba-0000-4000-8000-000000000000";
    for operation in ["6f1c1f3e-8f1b-4c55-9a55-4a3b8f0f6d2e", "operator:6f1c1f3e-8f1b-4c55-9a55-4a3b8f0f6d2e"] {
        assert!(Cli::try_parse_from(["chunk", "nodes", "shutdown", host, "--operation", operation]).is_ok());
    }
    assert!(Cli::try_parse_from(["chunk", "nodes", "shutdown", host, "--operation", "prep:1"]).is_err());
}

#[tokio::test]
async fn codegen_sets_up_editors_without_a_distribution_or_services() {
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join("server/schema")).unwrap();
    std::fs::write(project.path().join("server/schema/index.ts"), "unfinished schema").unwrap();
    let cli = Cli::try_parse_from(["chunk", "codegen", project.path().to_str().unwrap()]).unwrap();
    run(cli).await.unwrap();
    assert!(project.path().join(".chunk/generated/index.ts").is_file());
    assert!(!project.path().join(".chunk/build").exists());
    assert!(!project.path().join(".chunk/local").exists());
}
