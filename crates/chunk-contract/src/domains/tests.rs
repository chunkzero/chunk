use super::*;

fn manifest() -> DomainManifest {
    DomainManifest {
        version: DOMAIN_MANIFEST_VERSION,
        scopes: [
            (String::new(), DomainScope { parent: None }),
            ("games".into(), DomainScope { parent: Some(String::new()) }),
            ("games/duels".into(), DomainScope { parent: Some("games".into()) }),
        ]
        .into(),
        apps: [("duels".into(), "games/duels".into()), ("lobby".into(), String::new())].into(),
        hooks: BTreeMap::new(),
    }
}
fn hook(domain: &str, event: HookEvent, export: &str, order: Option<i32>) -> Hook {
    Hook { domain: domain.into(), event, export: export.into(), order, follow_player: false }
}

#[test]
fn static_ancestry_and_app_bindings_are_validated_at_the_contract_boundary() {
    let original = manifest();
    original.validate().unwrap();
    let encoded = serde_json::to_value(&original).unwrap();
    assert_eq!(serde_json::from_value::<DomainManifest>(encoded).unwrap(), original);
    for invalid in ["/games", "games/", "games//duels", "games/../duels", "games/[id]", "(group)", "a\\b"] {
        assert!(!domain_path(invalid), "{invalid}");
    }
    let mut invalid = original.clone();
    invalid.scopes.get_mut("games/duels").unwrap().parent = Some(String::new());
    assert_eq!(invalid.validate(), Err("invalid domain ancestry"));
    let mut invalid = original.clone();
    invalid.scopes.insert("Games".into(), DomainScope { parent: Some(String::new()) });
    assert!(invalid.validate().is_err());
    let mut invalid = original.clone();
    invalid.apps.insert("arena".into(), "missing".into());
    assert_eq!(invalid.validate(), Err("invalid app domain binding"));
    let mut invalid = original;
    invalid.version += 1;
    assert_eq!(invalid.validate(), Err("unsupported domain manifest version"));
}

#[test]
fn multiple_admission_hooks_require_distinct_explicit_order_within_each_scope() {
    let mut contract = manifest();
    contract.hooks.insert("shared/domains/hooks/checkBan".into(), hook("", HookEvent::PlayerLogin, "h0", None));
    contract
        .hooks
        .insert("shared/domains/games/hooks/checkRank".into(), hook("games", HookEvent::PlayerLogin, "h1", None));
    contract.validate().unwrap();
    contract.hooks.insert("shared/domains/hooks/checkCapacity".into(), hook("", HookEvent::PlayerLogin, "h2", None));
    assert_eq!(contract.validate(), Err("same-scope admission hooks require distinct explicit order values"));
    contract.hooks.get_mut("shared/domains/hooks/checkBan").unwrap().order = Some(10);
    contract.hooks.get_mut("shared/domains/hooks/checkCapacity").unwrap().order = Some(10);
    assert!(contract.validate().is_err());
    contract.hooks.get_mut("shared/domains/hooks/checkCapacity").unwrap().order = Some(20);
    contract.validate().unwrap();
    contract.hooks.get_mut("shared/domains/hooks/checkBan").unwrap().follow_player = true;
    assert_eq!(contract.validate(), Err("hook event cannot follow a player"));
}

#[test]
fn single_responders_require_root_and_notifications_do_not_imply_order() {
    let mut contract = manifest();
    contract.hooks.insert("shared/domains/hooks/status".into(), hook("", HookEvent::ServerPing, "h0", None));
    contract.validate().unwrap();
    contract.hooks.insert("shared/domains/hooks/other".into(), hook("", HookEvent::ServerPing, "h1", None));
    assert_eq!(contract.validate(), Err("ambiguous single-result hook responders"));
    contract.hooks.remove("shared/domains/hooks/other");
    contract.hooks.insert("shared/domains/games/hooks/route".into(), hook("games", HookEvent::PlayerRoute, "h2", None));
    assert_eq!(contract.validate(), Err("ping and routing responders require the root domain"));
    let notification = contract.hooks.get_mut("shared/domains/games/hooks/route").unwrap();
    notification.event = HookEvent::DomainEnter;
    notification.follow_player = true;
    contract.validate().unwrap();
    contract.hooks.get_mut("shared/domains/games/hooks/route").unwrap().order = Some(1);
    assert_eq!(contract.validate(), Err("only admission hooks accept ordering"));
}
