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
mod tests;
