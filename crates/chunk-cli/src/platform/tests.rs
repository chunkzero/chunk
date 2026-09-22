use super::*;

#[test]
fn override_does_not_read_saved_target() {
    let actual =
        resolve_target(Some("https://custom.example/api"), || panic!("must not read saved configuration")).unwrap();
    assert_eq!(actual, Target::Custom(Url::parse("https://custom.example/api").unwrap()));
    assert!(resolve_target(Some(""), || Ok(Target::Cloud)).is_err());
}

#[test]
fn target_urls_exclude_credentials_and_non_http_schemes() {
    for value in [
        "file:///tmp/platform",
        "https://user:secret@example.com",
        "https://example.com?token=secret",
        "https://example.com#fragment",
    ] {
        assert!(parse_url(value).is_err());
    }
    assert!(parse_url("http://localhost:8080/api").is_ok());
}
