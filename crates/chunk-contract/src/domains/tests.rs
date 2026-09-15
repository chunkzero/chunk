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
        commands: BTreeMap::new(),
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

#[test]
fn command_metadata_preserves_legacy_encoding_and_rejects_invalid_identity_or_exports() {
    let mut contract = manifest();
    let legacy = serde_json::to_value(&contract).unwrap();
    assert!(legacy.get("commands").is_none());
    let decoded: DomainManifest = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), legacy);
    let command = crate::Command {
        domain: String::new(),
        name: "hub".into(),
        aliases: Vec::new(),
        export: "sharedExport".into(),
        permission: None,
        follow_player: false,
        routes: vec![crate::CommandRoute { literals: Vec::new(), arguments: Vec::new() }],
    };
    contract.commands.insert("shared/domains/commands/hub".into(), command.clone());
    contract.validate().unwrap();
    let mut invalid = contract.clone();
    invalid.commands.insert("wrong/hub".into(), command);
    assert_eq!(invalid.validate(), Err("invalid command identity, export or domain"));
    let mut deployment = crate::Deployment {
        session_methods: None,
        session_configurations: None,
        destinations: None,
        contract_version: crate::CONTRACT_VERSION,
        runtime_profile: crate::RuntimeProfile::TransactionalV1,
        id: "domain-commands".into(),
        source: "unused".into(),
        tables: BTreeMap::new(),
        functions: [(
            "query".into(),
            crate::Function {
                kind: crate::FunctionKind::Query,
                visibility: crate::Visibility::Internal,
                export: "sharedExport".into(),
                arguments: crate::Schema::Object { fields: BTreeMap::new() },
                result: crate::Schema::Boolean,
            },
        )]
        .into(),
        domains: Some(contract),
    };
    assert_eq!(deployment.validate(), Err("domain handler export collides with function export"));
    deployment.functions.get_mut("query").unwrap().export = "queryExport".into();
    let domain = deployment.domains.as_mut().unwrap();
    domain.commands.get_mut("shared/domains/commands/hub").unwrap().permission = Some("query".into());
    deployment.validate().unwrap();
    deployment.functions.get_mut("query").unwrap().kind = crate::FunctionKind::Mutation;
    assert_eq!(deployment.validate(), Err("unknown command query reference"));
    deployment.functions.get_mut("query").unwrap().kind = crate::FunctionKind::Query;
    deployment.domains.as_mut().unwrap().commands.get_mut("shared/domains/commands/hub").unwrap().routes[0]
        .arguments
        .push(crate::CommandArgument {
            name: "target".into(),
            parser: crate::CommandParser::Word,
            min: None,
            max: None,
            suggestions: Some(crate::CommandSuggestions::Query(crate::SuggestionQuery { query: "query".into() })),
        });
    assert_eq!(
        deployment.validate(),
        Err("command suggestions require input/cursor query arguments and string array results")
    );
}
