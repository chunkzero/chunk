use super::config::{Config, Secret, Target, load_from, parse_url, resolve, save_to};
use super::*;
use url::Url;

fn custom(url: &str) -> Target {
    Target::Custom(Url::parse(url).unwrap())
}

fn saved(target: Target) -> Config {
    Config { target, token: Some(Secret::new("chunk_saved".into())) }
}

#[test]
fn environment_token_and_url_need_no_saved_login() {
    let credentials = resolve(Some("https://custom.example/api"), Some("chunk_ci".into()), || {
        panic!("must not read saved configuration")
    })
    .unwrap();
    assert_eq!(credentials.target, custom("https://custom.example/api"));
    assert_eq!(credentials.token.unwrap().expose(), "chunk_ci");
    assert!(credentials.from_env);
    assert!(resolve(Some(""), None, || Ok(Config::default())).is_err());
}

#[test]
fn the_saved_token_only_goes_to_its_own_platform() {
    let own = resolve(Some("https://custom.example/"), None, || Ok(saved(custom("https://custom.example/")))).unwrap();
    assert_eq!(own.token.unwrap().expose(), "chunk_saved");
    let other = resolve(Some("https://other.example/"), None, || Ok(saved(custom("https://custom.example/")))).unwrap();
    assert_eq!(other.target, custom("https://other.example/"));
    assert!(other.token.is_none());
    let cloud = resolve(Some("https://API.chunkzero.com/"), None, || Ok(saved(Target::Cloud))).unwrap();
    assert_eq!((cloud.target, cloud.token.unwrap().expose()), (Target::Cloud, "chunk_saved"));
    let slash = resolve(Some("https://custom.example/api/"), None, || Ok(saved(custom("https://custom.example/api"))));
    assert_eq!(slash.unwrap().token.unwrap().expose(), "chunk_saved");
    let overridden = resolve(None, Some("chunk_ci".into()), || Ok(saved(Target::Cloud))).unwrap();
    assert_eq!((overridden.target, overridden.token.unwrap().expose()), (Target::Cloud, "chunk_ci"));
}

#[test]
fn saved_logins_are_private_and_never_debug_print_the_token() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("chunk/config.json");
    assert_eq!(load_from(&path).unwrap(), Config::default());
    let config = saved(custom("http://localhost:8080/"));
    save_to(&path, &config).unwrap();
    assert_eq!(load_from(&path).unwrap(), config);
    assert!(!format!("{config:?}").contains("chunk_saved"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = || std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(), 0o600);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(load_from(&path).unwrap(), config);
        assert_eq!(mode(), 0o600);
    }
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

#[test]
fn projects_and_environments_are_chosen_by_name_or_id() {
    let key: fn(&(String, String)) -> [&String; 2] = |(id, name)| [id, name];
    let items = || vec![("prj_1".to_owned(), "lobby".to_owned()), ("prj_2".to_owned(), "arena".to_owned())];
    assert_eq!(choose(items(), Some("arena"), "project", key).unwrap().0, "prj_2");
    assert_eq!(choose(items(), Some("prj_1"), "project", key).unwrap().1, "lobby");
    let error = choose(items(), None, "project", key).unwrap_err().to_string();
    assert!(error.contains("--project") && error.contains("lobby, arena"), "{error}");
    assert!(choose(items(), Some("missing"), "project", key).is_err());
    assert_eq!(choose(items().split_off(1), None, "project", key).unwrap().1, "arena");
    assert!(choose(Vec::new(), None, "project", key).unwrap_err().to_string().contains("chunk projects create"));
    let shared = vec![("prj_1".to_owned(), "demo".to_owned()), ("prj_2".to_owned(), "demo".to_owned())];
    let error = choose(shared.clone(), Some("demo"), "project", key).unwrap_err().to_string();
    assert!(error.contains("prj_1, prj_2"), "{error}");
    assert_eq!(choose(shared, Some("prj_2"), "project", key).unwrap().0, "prj_2");
}
