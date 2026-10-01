use super::*;

fn options(project: &Path) -> Options {
    Options {
        project: project.into(),
        target: Target::Java,
        output: None,
        backend_output: None,
        java_package: None,
        frozen: false,
    }
}

#[test]
fn generation_validates_shared_metadata_and_output_separation_before_compiling() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("chunk.toml"), "domains = []").unwrap();
    let error = run(options(project.path())).unwrap_err();
    assert!(error.to_string().contains("unknown field"), "{error}");
    assert!(!project.path().join(".chunk").exists());
    std::fs::write(project.path().join("chunk.toml"), "").unwrap();
    let mut overlapping = options(project.path());
    overlapping.output = Some(project.path().join(".chunk/build/backend/../backend"));
    let error = run(overlapping).unwrap_err();
    assert!(error.to_string().contains("must be separate"), "{error}");
    assert!(!project.path().join(".chunk").exists());
}
