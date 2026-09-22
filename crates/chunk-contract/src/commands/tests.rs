use super::*;

fn command(domain: &str, name: &str) -> Command {
    Command {
        domain: domain.into(),
        name: name.into(),
        aliases: vec![],
        export: "handler".into(),
        permission: None,
        follow_player: false,
        routes: vec![CommandRoute { literals: vec![], arguments: vec![] }],
    }
}

#[test]
fn inherited_commands_have_one_owner_and_siblings_are_independent() {
    let mut commands = BTreeMap::from([
        ("root".into(), command("", "hub")),
        ("duels".into(), command("minigames/duels", "leave")),
        ("races".into(), command("minigames/races", "leave")),
    ]);
    let visible = visible_commands(&commands, "minigames/duels", &[]).unwrap();
    assert_eq!(visible, BTreeMap::from([("hub", "root"), ("leave", "duels")]));
    assert!(visible_commands(&commands, "minigames/duels", &["hub".into()]).is_err());
    commands.get_mut("duels").unwrap().aliases.push("hub".into());
    assert!(visible_commands(&commands, "minigames/duels", &[]).is_err());
    assert!(visible_commands(&commands, "minigames/races", &[]).is_ok());
}

#[test]
fn command_child_names_cannot_alias_literal_and_argument_nodes() {
    let mut grammar = command("", "travel");
    grammar.routes = vec![
        CommandRoute {
            literals: vec!["admin".into()],
            arguments: vec![CommandArgument {
                name: "list".into(),
                parser: CommandParser::Word,
                min: None,
                max: None,
                suggestions: None,
            }],
        },
        CommandRoute { literals: vec!["admin".into(), "list".into()], arguments: vec![] },
    ];
    assert!(grammar.validate(&BTreeMap::new()).is_err());
    grammar.routes[1].literals[0] = "public".into();
    assert!(grammar.validate(&BTreeMap::new()).is_ok());
}

#[test]
fn malformed_command_grammar_is_rejected_at_the_contract_boundary() {
    let original = command("", "reward");
    original.validate(&BTreeMap::new()).unwrap();
    let mut duplicate = original.clone();
    duplicate.routes.push(original.routes[0].clone());
    assert!(duplicate.validate(&BTreeMap::new()).is_err());
    let mut bounds = original;
    bounds.routes[0].arguments.push(CommandArgument {
        name: "message".into(),
        parser: CommandParser::Word,
        min: Some(1),
        max: None,
        suggestions: None,
    });
    assert!(bounds.validate(&BTreeMap::new()).is_err());
}
