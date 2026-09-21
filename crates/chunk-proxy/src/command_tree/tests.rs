use super::*;
use chunk_contract::{CommandArgument, CommandRoute, CommandSuggestions, SuggestionQuery};
use chunk_protocol::{Decode, VarInt, decode_packet};

fn command() -> Command {
    Command {
        domain: "hub".into(),
        name: "travel".into(),
        aliases: vec!["go".into()],
        export: "travel".into(),
        permission: None,
        follow_player: false,
        routes: vec![CommandRoute {
            literals: vec![],
            arguments: vec![CommandArgument {
                name: "destination".into(),
                parser: CommandParser::String,
                min: None,
                max: None,
                suggestions: Some(CommandSuggestions::Static(vec![
                    "Alpha".into(),
                    "Another world".into(),
                    "🎮 arena".into(),
                ])),
            }],
        }],
    }
}
fn published(catalog: &CommandTreeCatalog, visible: impl Fn(&str, &Command) -> bool) -> CommandTree {
    let frame = catalog.merge(visible).unwrap();
    let mut body = frame.as_slice();
    VarInt::decode(&mut body).unwrap();
    decode_packet(body).unwrap()
}
fn catalog(command: Command) -> CommandTreeCatalog {
    CommandTreeCatalog::new(CommandTree::empty(), &BTreeMap::from([("travel".into(), command)]), "hub/lobby").unwrap()
}

#[test]
fn ownership_precedes_permissions_and_preserves_jvm_indices_and_redirects() {
    let mut jvm = CommandTree::empty();
    let mut root = CommandNode::new(literal("vanilla").unwrap());
    root.redirect = Some(0);
    root.restricted = true;
    jvm.nodes.push(root);
    jvm.nodes[0].children.push(1);
    let commands = BTreeMap::from([("travel".into(), command())]);
    let catalog = CommandTreeCatalog::new(jvm.clone(), &commands, "hub/lobby").unwrap();
    assert_eq!(published(&catalog, |_, _| false), jvm);
    assert_eq!(catalog.owner("go malformed arguments").unwrap().0, "travel");
    let published = published(&catalog, |_, _| true);
    assert_eq!(published.nodes[1], jvm.nodes[1]);
    assert_eq!(published.nodes[0].children[0], 1);
    let alias = published.nodes.iter().find(|node| node.name() == Some("go")).unwrap();
    let primary = alias.redirect.unwrap();
    assert_eq!(published.nodes[primary].name(), Some("travel"));
    let argument = &published.nodes[published.nodes[primary].children[0]];
    assert!(
        matches!(&argument.kind, NodeKind::Argument { suggestions: Some(id), .. } if id.as_str() == "minecraft:ask_server")
    );
    let mut collision = command();
    collision.aliases.push("vanilla".into());
    assert!(
        CommandTreeCatalog::new(jvm.clone(), &BTreeMap::from([("travel".into(), collision)]), "hub/lobby").is_err()
    );
    let elsewhere = CommandTreeCatalog::new(jvm, &commands, "arena").unwrap();
    assert!(elsewhere.owner("travel").is_none());
    assert_eq!(catalog.parse("go Alpha").unwrap().1.arguments["destination"], "Alpha");
    assert!(catalog.parse("go Alpha extra").is_err());
}

#[test]
fn suggestions_preserve_utf16_ranges_and_apply_one_filter_to_static_and_query_values() {
    let mut command = command();
    command.routes[0].arguments.insert(
        0,
        CommandArgument {
            name: "label".into(),
            parser: CommandParser::String,
            min: None,
            max: None,
            suggestions: None,
        },
    );
    let catalog = catalog(command.clone());
    let input = "/travel \"🎮\" An";
    let cursor = u32::try_from(input.encode_utf16().count()).unwrap();
    let plan = catalog.suggestions(input, cursor, |_, _| true).unwrap().unwrap();
    assert_eq!((plan.start, plan.length), (13, 2));
    let response = plan.finish(4, &[]).unwrap();
    assert_eq!(response.matches[0].as_str(), "\"Another world\"");
    assert!(catalog.suggestions(input, 10, |_, _| true).is_err()); // Splits the emoji surrogate pair.
    assert!(catalog.suggestions(input, cursor, |_, _| false).unwrap().is_none());
    command.routes[0].arguments[1].suggestions =
        Some(CommandSuggestions::Query(SuggestionQuery { query: "places".into() }));
    let dynamic = super::tests::catalog(command).suggestions(input, cursor, |_, _| true).unwrap().unwrap();
    assert_eq!(dynamic.query.as_deref(), Some("places"));
    assert_eq!(dynamic.input, input);
    assert_eq!(dynamic.cursor, cursor);
    assert_eq!(dynamic.finish(4, &["Another world".into(), "Beta".into()]).unwrap(), response);
    assert!(dynamic.finish(4, &vec!["value".into(); 65]).is_err());
    assert!(dynamic.finish(4, &["bad\nvalue".into()]).is_err());
}

#[test]
fn literal_precedence_quotes_and_mid_token_ranges_match_the_published_grammar() {
    let mut command = command();
    command.routes.push(CommandRoute { literals: vec!["admin".into(), "list".into()], arguments: vec![] });
    let catalog = catalog(command);
    let plan = catalog.suggestions("/go admin l", 11, |_, _| true).unwrap().unwrap();
    assert_eq!(plan.finish(1, &[]).unwrap().matches[0].as_str(), "list");
    assert!(catalog.suggestions("/go admin other ", 16, |_, _| true).unwrap().is_none());
    let quoted = catalog.suggestions("/go '🎮 ar'", 10, |_, _| true).unwrap().unwrap();
    assert_eq!((quoted.start, quoted.length), (4, 7));
    assert_eq!(quoted.finish(1, &[]).unwrap().matches[0].as_str(), "'🎮 arena'");
    let mid = catalog.suggestions("/go Alxyz", 6, |_, _| true).unwrap().unwrap();
    assert_eq!((mid.start, mid.length), (4, 5));
    assert_eq!(mid.finish(1, &[]).unwrap().matches[0].as_str(), "Alpha");
    let roots = catalog.suggestions("/tr", 3, |_, _| true).unwrap().unwrap();
    assert_eq!((roots.start, roots.length), (1, 2));
    assert_eq!(roots.finish(1, &[]).unwrap().matches[0].as_str(), "travel");
}
