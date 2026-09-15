use super::{CommandTreeCatalog, Result};
use chunk_contract::{Command, CommandParser, CommandRoute, CommandSuggestions as Values};
use chunk_protocol::{McString, commands::CommandSuggestions};
use std::collections::BTreeSet;

/// Query inputs retain the original slash and UTF-16 cursor. Ranges replace the active token,
/// including its suffix after the cursor when the supplied input contains one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestionPlan {
    pub query: Option<String>,
    pub input: String,
    pub cursor: u32,
    pub start: u32,
    pub length: u32,
    prefix: String,
    parser: CommandParser,
    quote: Option<char>,
    values: Vec<String>,
}
impl SuggestionPlan {
    /// Applies the same limits, prefix filtering and quoting to static and query results.
    /// # Errors
    /// Rejects excessive/malformed query results and strings outside packet limits.
    pub fn finish(&self, transaction_id: i32, query_values: &[String]) -> Result<CommandSuggestions> {
        if query_values.len() > 64 || query_values.iter().any(|value| !valid_value(value)) {
            return Err("invalid command suggestion result");
        }
        let prefix = self.prefix.to_lowercase();
        let mut matches = BTreeSet::new();
        for value in self.values.iter().chain(query_values) {
            if !value.to_lowercase().starts_with(&prefix) {
                continue;
            }
            let rendered = match self.parser {
                CommandParser::String => {
                    if self.quote.is_none() && unquoted(value) {
                        value.clone()
                    } else {
                        let quote = self.quote.unwrap_or('"');
                        let escaped = value.replace('\\', "\\\\").replace(quote, &format!("\\{quote}"));
                        format!("{quote}{escaped}{quote}")
                    }
                }
                CommandParser::Word | CommandParser::Boolean if !unquoted(value) => continue,
                _ => value.clone(),
            };
            matches.insert(rendered);
        }
        let matches = matches
            .into_iter()
            .take(64)
            .map(|value| McString::new(value).map_err(|_| "suggestion wire limit"))
            .collect::<Result<Vec<_>>>()?;
        Ok(CommandSuggestions { transaction_id, start: self.start, length: self.length, matches })
    }
}
impl CommandTreeCatalog {
    /// Plans suggestions from validated descriptors and already-resolved permission results.
    /// # Errors
    /// Rejects excessive input, controls and a cursor splitting a UTF-16 surrogate pair.
    pub fn suggestions(
        &self,
        input: &str,
        cursor: u32,
        visible: impl Fn(&str, &Command) -> bool,
    ) -> Result<Option<SuggestionPlan>> {
        let slash = usize::from(input.starts_with('/'));
        if input.len() > 3075 || input.encode_utf16().count() > 1024 + slash || input.chars().any(char::is_control) {
            return Err("invalid command suggestion input");
        }
        let cursor_byte = byte_cursor(input, cursor)?;
        if cursor_byte < slash {
            return Ok(None);
        }
        let before = &input[..cursor_byte];
        let body = &before[slash..];
        let Some(root_end) = body.find(' ') else {
            let values = self
                .owners
                .iter()
                .filter(|(_, id)| visible(id, &self.commands[*id]))
                .map(|(root, _)| root.clone())
                .collect();
            return Ok(Some(plan(input, cursor, slash, CommandParser::Word, values, None)?));
        };
        let root = &body[..root_end];
        let Some(id) = self.owners.get(root) else {
            return Ok(None);
        };
        let command = &self.commands[id];
        if !visible(id, command) {
            return Ok(None);
        }
        let mut candidates: Vec<_> = command.routes.iter().collect();
        let mut start = slash + root_end + 1;
        let mut depth = 0;
        loop {
            let remaining = &before[start..];
            let end = remaining.find(' ');
            let token = end.map_or(remaining, |end| &remaining[..end]);
            let matching: Vec<_> = candidates
                .iter()
                .copied()
                .filter(|route| route.literals.get(depth).is_some_and(|literal| literal == token))
                .collect();
            if let Some(end) = end
                && !matching.is_empty()
            {
                candidates = matching;
                depth += 1;
                start += end + 1;
                continue;
            }
            let literals: Vec<_> = if end.is_none() {
                candidates.iter().filter_map(|route| route.literals.get(depth).cloned()).collect()
            } else {
                Vec::new()
            };
            let route = candidates.into_iter().find(|route| route.literals.len() == depth);
            if let Some(route) = route {
                return argument_plan(command, route, input, cursor, start, literals);
            }
            if end.is_none() {
                return Ok(Some(plan(input, cursor, start, CommandParser::Word, literals, None)?));
            }
            return Ok(None);
        }
    }
}

fn argument_plan(
    command: &Command,
    route: &CommandRoute,
    input: &str,
    cursor: u32,
    mut start: usize,
    literals: Vec<String>,
) -> Result<Option<SuggestionPlan>> {
    let cursor_byte = byte_cursor(input, cursor)?;
    let before = &input[..cursor_byte];
    if route.arguments.is_empty() {
        return if literals.is_empty() {
            Ok(None)
        } else {
            Ok(Some(plan(input, cursor, start, CommandParser::Word, literals, None)?))
        };
    }
    for (index, argument) in route.arguments.iter().enumerate() {
        let end = token_end(before, start, argument.parser);
        if end < before.len() && before.as_bytes()[end] == b' ' {
            let mut grammar = command.clone();
            grammar.routes =
                vec![CommandRoute { literals: route.literals.clone(), arguments: route.arguments[..=index].to_vec() }];
            if grammar.parse(before[..end].strip_prefix('/').unwrap_or(&before[..end])).is_err() {
                return Ok(None);
            }
            start = end + 1;
            continue;
        }
        let (mut values, query) = match &argument.suggestions {
            Some(Values::Static(values)) => (values.clone(), None),
            Some(Values::Query(reference)) => (Vec::new(), Some(reference.query.clone())),
            None if argument.parser == CommandParser::Boolean => (vec!["true".into(), "false".into()], None),
            None => (Vec::new(), None),
        };
        if index == 0 {
            values.extend(literals);
        }
        return Ok(Some(plan(input, cursor, start, argument.parser, values, query)?));
    }
    Ok(None)
}

fn plan(
    input: &str,
    cursor: u32,
    start: usize,
    parser: CommandParser,
    values: Vec<String>,
    query: Option<String>,
) -> Result<SuggestionPlan> {
    let cursor_byte = byte_cursor(input, cursor)?;
    let raw = &input[start..cursor_byte];
    let quote =
        if parser == CommandParser::String { raw.chars().next().filter(|ch| matches!(ch, '\'' | '"')) } else { None };
    let prefix = if let Some(quote) = quote {
        let mut value = String::new();
        let mut escaped = false;
        for ch in raw[1..].chars() {
            if escaped {
                if ch != quote && ch != '\\' {
                    return Err("invalid suggestion escape");
                }
                value.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                break;
            } else {
                value.push(ch);
            }
        }
        value
    } else {
        raw.to_owned()
    };
    let end = token_end(input, start, parser).max(cursor_byte);
    let start = u32::try_from(input[..start].encode_utf16().count()).map_err(|_| "suggestion range limit")?;
    let end = u32::try_from(input[..end].encode_utf16().count()).map_err(|_| "suggestion range limit")?;
    Ok(SuggestionPlan {
        query,
        input: input.to_owned(),
        cursor,
        start,
        length: end - start,
        prefix,
        parser,
        quote,
        values,
    })
}
fn byte_cursor(input: &str, cursor: u32) -> Result<usize> {
    let mut units = 0;
    for (offset, ch) in input.char_indices() {
        if units == cursor {
            return Ok(offset);
        }
        units += u32::try_from(ch.len_utf16()).map_err(|_| "invalid cursor")?;
    }
    if units == cursor { Ok(input.len()) } else { Err("invalid UTF-16 suggestion cursor") }
}
fn token_end(input: &str, start: usize, parser: CommandParser) -> usize {
    if parser == CommandParser::Greedy {
        return input.len();
    }
    let rest = &input[start..];
    if matches!(parser, CommandParser::String | CommandParser::Boolean)
        && let Some(quote @ ('\'' | '"')) = rest.chars().next()
    {
        let mut escaped = false;
        for (offset, ch) in rest[1..].char_indices() {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                return start + offset + 2;
            }
        }
        input.len()
    } else {
        start + rest.find(' ').unwrap_or(rest.len())
    }
}
fn unquoted(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'+'))
}
fn valid_value(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && value.chars().count() <= 256 && !value.chars().any(char::is_control)
}
