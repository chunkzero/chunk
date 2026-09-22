use super::*;
use crate::CommandRoute;

fn command(parsers: &[CommandParser]) -> Command {
    Command {
        domain: String::new(),
        name: "test".into(),
        aliases: vec!["t".into()],
        export: "handler".into(),
        permission: None,
        follow_player: false,
        routes: vec![CommandRoute {
            literals: vec![],
            arguments: parsers
                .iter()
                .enumerate()
                .map(|(i, parser)| CommandArgument {
                    name: format!("arg{i}"),
                    parser: *parser,
                    min: None,
                    max: None,
                    suggestions: None,
                })
                .collect(),
        }],
    }
}

#[test]
fn parses_aliases_bounded_numbers_and_brigadier_strings() {
    let mut grammar =
        command(&[CommandParser::Boolean, CommandParser::Integer, CommandParser::String, CommandParser::Greedy]);
    grammar.routes[0].arguments[1].min = Some(-2);
    grammar.routes[0].arguments[1].max = Some(2);
    let parsed = grammar.parse(r#"t true -2 "hello \"世界\"" two more words"#).unwrap();
    assert_eq!(parsed.route, 0);
    assert_eq!(
        Value::Object(parsed.arguments),
        serde_json::json!({
            "arg0": true, "arg1": -2, "arg2": "hello \"世界\"", "arg3": "two more words",
        })
    );
    assert!(grammar.parse("test false 3 word rest").is_err());
    assert!(grammar.parse("test false +1 word rest").is_err());
    assert_eq!(command(&[CommandParser::String]).parse("test ''").unwrap().arguments["arg0"], "");
    assert_eq!(command(&[CommandParser::Boolean]).parse("test 'true'").unwrap().arguments["arg0"], true);
    assert_eq!(command(&[CommandParser::Greedy]).parse("test  two words").unwrap().arguments["arg0"], " two words");
    for input in ["test 'oops", "test 'a\\n'", "test 'a'next", "test word!", "test x extra", "/test word", "test x "] {
        assert!(command(&[CommandParser::String]).parse(input).is_err(), "{input}");
    }
}

#[test]
fn literal_branches_win_even_when_the_selected_route_is_incomplete() {
    let mut grammar = command(&[CommandParser::Greedy]);
    grammar.routes.push(CommandRoute { literals: vec!["admin".into(), "list".into()], arguments: vec![] });
    assert_eq!(grammar.parse("test admin list").unwrap().route, 1);
    assert!(grammar.parse("test admin").is_err());
    assert!(grammar.parse("test admin other").is_err());
    assert_eq!(grammar.parse("test ordinary words").unwrap().route, 0);
    let excess = format!("test {}", "🎮".repeat(MAX_COMMAND_INPUT / 2));
    assert!(grammar.parse(&excess).is_err());
    assert!(grammar.parse("test a\nb").is_err());
}

#[test]
fn quoted_scanning_reports_ends_and_escapes_independently() {
    assert_eq!(quoted("word"), None);
    assert_eq!(quoted(r#""a\"b" rest"#), Some(Quoted { value: Ok("a\"b".into()), end: Some(6) }));
    assert_eq!(quoted("'open"), Some(Quoted { value: Ok("open".into()), end: None }));
    assert_eq!(quoted(r"'a\n' x"), Some(Quoted { value: Err("invalid quoted command escape"), end: Some(5) }));
    assert_eq!(quoted(r"'a\"), Some(Quoted { value: Ok("a".into()), end: None }));
}
