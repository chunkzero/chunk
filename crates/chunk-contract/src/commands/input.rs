use serde_json::{Map, Value};

use super::{Command, CommandArgument, CommandParser};

/// Local input bound, measured in Minecraft/Java UTF-16 code units.
pub const MAX_COMMAND_INPUT: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    pub route: usize,
    pub arguments: Map<String, Value>,
}

impl Command {
    /// Parses an unsigned command body without its leading slash. Literals take
    /// precedence over argument routes, matching the published command tree.
    /// # Errors
    /// Rejects the wrong root, malformed/extra arguments, excessive input and
    /// values outside the declared parser bounds. Does not authorize dispatch.
    pub fn parse(&self, input: &str) -> Result<ParsedCommand, &'static str> {
        if input.len() > MAX_COMMAND_INPUT * 3
            || input.encode_utf16().count() > MAX_COMMAND_INPUT
            || input.chars().any(char::is_control)
        {
            return Err("invalid or excessive command input");
        }
        let mut reader = Reader(input);
        let root = reader.token();
        if root != self.name && !self.aliases.iter().any(|alias| alias == root) {
            return Err("command root does not match descriptor");
        }
        reader.separator()?;
        let mut candidates: Vec<_> = self.routes.iter().enumerate().collect();
        let mut depth = 0;
        while !reader.0.is_empty() {
            let mut next = reader;
            let token = next.token();
            let matching: Vec<_> = candidates
                .iter()
                .copied()
                .filter(|(_, route)| route.literals.get(depth).is_some_and(|literal| literal == token))
                .collect();
            if matching.is_empty() {
                break;
            }
            candidates = matching;
            depth += 1;
            reader = next;
            reader.separator()?;
        }
        let (route, grammar) = candidates
            .into_iter()
            .find(|(_, route)| route.literals.len() == depth)
            .ok_or("incomplete command literal route")?;
        let mut arguments = Map::new();
        for argument in &grammar.arguments {
            if reader.0.is_empty() {
                return Err("missing command argument");
            }
            arguments.insert(argument.name.clone(), reader.argument(argument)?);
            reader.separator()?;
        }
        if !reader.0.is_empty() {
            return Err("unexpected command input");
        }
        Ok(ParsedCommand { route, arguments })
    }
}

#[derive(Clone, Copy)]
struct Reader<'a>(&'a str);

impl<'a> Reader<'a> {
    fn separator(&mut self) -> Result<(), &'static str> {
        if !self.0.is_empty() {
            self.0 = self.0.strip_prefix(' ').ok_or("command arguments must be separated by spaces")?;
            if self.0.is_empty() {
                return Err("unexpected trailing command separator");
            }
        }
        Ok(())
    }

    fn token(&mut self) -> &'a str {
        let end = self.0.find(' ').unwrap_or(self.0.len());
        let token = &self.0[..end];
        self.0 = &self.0[end..];
        token
    }

    fn argument(&mut self, argument: &CommandArgument) -> Result<Value, &'static str> {
        match argument.parser {
            CommandParser::Boolean => match self.string()?.as_str() {
                "true" => Ok(Value::Bool(true)),
                "false" => Ok(Value::Bool(false)),
                _ => Err("expected boolean command argument"),
            },
            CommandParser::Integer => {
                let token = self.token();
                if !token.bytes().all(|byte| byte.is_ascii_digit() || byte == b'-') {
                    return Err("expected integer command argument");
                }
                let value: i32 = token.parse().map_err(|_| "expected integer command argument")?;
                if argument.min.is_some_and(|min| value < min) || argument.max.is_some_and(|max| value > max) {
                    return Err("command integer outside declared bounds");
                }
                Ok(value.into())
            }
            CommandParser::Word => self.word().map(Into::into),
            CommandParser::String => self.string().map(Into::into),
            CommandParser::Greedy => {
                let value = std::mem::take(&mut self.0);
                Ok(value.into())
            }
        }
    }

    fn word(&mut self) -> Result<&'a str, &'static str> {
        let word = self.token();
        if !unquoted_word(word) {
            return Err("expected unquoted command word");
        }
        Ok(word)
    }

    fn string(&mut self) -> Result<String, &'static str> {
        let Some(Quoted { value, end }) = quoted(self.0) else {
            return self.word().map(str::to_owned);
        };
        let value = value?;
        self.0 = &self.0[end.ok_or("unterminated quoted command argument")?..];
        Ok(value)
    }
}

/// Whether `value` is a non-empty Brigadier word that needs no quoting.
#[must_use]
pub fn unquoted_word(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'+'))
}

/// A Brigadier quoted string scanned from the opening quote at the start of the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quoted {
    /// The unescaped value, or the first invalid escape.
    pub value: Result<String, &'static str>,
    /// Byte offset just past the closing quote, or `None` when unterminated.
    pub end: Option<usize>,
}

/// Scans a quoted string, returning `None` unless `input` opens with a quote.
#[must_use]
pub fn quoted(input: &str) -> Option<Quoted> {
    let quote @ ('\'' | '"') = input.chars().next()? else {
        return None;
    };
    let mut value = Ok(String::new());
    let mut escaped = false;
    for (offset, ch) in input[1..].char_indices() {
        if escaped {
            escaped = false;
            if ch != quote && ch != '\\' {
                value = Err("invalid quoted command escape");
            } else if let Ok(value) = &mut value {
                value.push(ch);
            }
        } else if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            return Some(Quoted { value, end: Some(offset + 2) });
        } else if let Ok(value) = &mut value {
            value.push(ch);
        }
    }
    Some(Quoted { value, end: None })
}

#[cfg(test)]
mod tests {
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
        for input in
            ["test 'oops", "test 'a\\n'", "test 'a'next", "test word!", "test x extra", "/test word", "test x "]
        {
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
}
