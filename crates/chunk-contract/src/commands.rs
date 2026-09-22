use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{Function, FunctionKind, Schema, deployment::ascii_identifier};

mod input;
pub use input::{MAX_COMMAND_INPUT, ParsedCommand, Quoted, quoted, unquoted_word};

/// A backend-owned command root and its literal/argument routes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub domain: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub export: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<String>,
    pub follow_player: bool,
    pub routes: Vec<CommandRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRoute {
    pub literals: Vec<String>,
    pub arguments: Vec<CommandArgument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandArgument {
    pub name: String,
    pub parser: CommandParser,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestions: Option<CommandSuggestions>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandParser {
    Boolean,
    Integer,
    Word,
    String,
    Greedy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CommandSuggestions {
    Static(Vec<String>),
    Query(SuggestionQuery),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuggestionQuery {
    pub query: String,
}

impl Command {
    /// Validates the parser grammar and the permission/suggestion query contracts.
    /// # Errors
    /// Rejects ambiguous routes, unsupported arguments, and incompatible query references.
    pub fn validate(&self, functions: &BTreeMap<String, Function>) -> Result<(), &'static str> {
        let mut aliases = BTreeSet::new();
        if self.aliases.len() > 16
            || !std::iter::once(&self.name).chain(&self.aliases).all(|name| literal(name) && aliases.insert(name))
        {
            return Err("invalid or duplicate command root/alias");
        }
        if let Some(path) = &self.permission {
            let function = query(functions, path)?;
            if !matches!(&function.arguments, Schema::Object { fields } if fields.is_empty())
                || function.result != Schema::Boolean
            {
                return Err("command permission must be a query with empty arguments returning boolean");
            }
        }
        let mut paths = BTreeSet::new();
        if self.routes.is_empty() || self.routes.len() > 64 {
            return Err("command route count limit");
        }
        let mut nodes = 1 + self.aliases.len();
        for route in &self.routes {
            nodes += route.literals.len() + route.arguments.len();
            if nodes > 256
                || route.literals.len() > 8
                || !route.literals.iter().all(|name| literal(name))
                || !paths.insert(&route.literals)
                || route.arguments.len() > 16
            {
                return Err("invalid, duplicate or excessive command route");
            }
            if let Some(argument) = route.arguments.first()
                && self.routes.iter().any(|other| {
                    other.literals.starts_with(&route.literals)
                        && other.literals.get(route.literals.len()) == Some(&argument.name)
                })
            {
                return Err("command argument name conflicts with literal child");
            }
            let mut arguments = BTreeSet::new();
            for (index, argument) in route.arguments.iter().enumerate() {
                if !ascii_identifier(&argument.name, 64) || !arguments.insert(argument.name.to_ascii_lowercase()) {
                    return Err("invalid or duplicate command argument name");
                }
                if argument.parser == CommandParser::Greedy && index + 1 != route.arguments.len() {
                    return Err("greedy command argument must be last");
                }
                argument.validate(functions)?;
            }
        }
        Ok(())
    }
}

impl CommandArgument {
    fn validate(&self, functions: &BTreeMap<String, Function>) -> Result<(), &'static str> {
        if self.parser != CommandParser::Integer && (self.min.is_some() || self.max.is_some()) {
            return Err("only integer command arguments accept numeric bounds");
        }
        if self.min.zip(self.max).is_some_and(|(min, max)| min > max) {
            return Err("command integer minimum exceeds maximum");
        }
        if self.suggestions.is_some() && matches!(self.parser, CommandParser::Integer | CommandParser::Boolean) {
            return Err("custom command suggestions require a string parser");
        }
        match &self.suggestions {
            Some(CommandSuggestions::Static(values)) => {
                let mut seen = BTreeSet::new();
                if values.len() > 64 || !values.iter().all(|value| valid_suggestion(value) && seen.insert(value)) {
                    return Err("invalid command suggestions");
                }
            }
            Some(CommandSuggestions::Query(reference)) => {
                let function = query(functions, &reference.query)?;
                if !matches!(&function.arguments, Schema::Object { fields }
                    if fields.len() == 2
                        && fields.get("input").is_some_and(|field| !field.optional && field.schema == Schema::String)
                        && fields.get("cursor").is_some_and(|field| !field.optional && field.schema == Schema::Integer))
                    || !matches!(&function.result, Schema::Array { items } if **items == Schema::String)
                {
                    return Err("command suggestions require input/cursor query arguments and string array results");
                }
            }
            None => {}
        }
        Ok(())
    }
}

/// Whether a static or query suggestion value fits the command suggestion limits.
#[must_use]
pub fn valid_suggestion(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && value.chars().count() <= 256 && !value.chars().any(char::is_control)
}

/// Selects inherited command roots and rejects conflicts with app-local JVM commands.
/// This validates ownership; callers still authorize command visibility and dispatch.
/// # Errors
/// Rejects any root/alias with more than one visible owner.
pub fn visible_commands<'a>(
    commands: &'a BTreeMap<String, Command>,
    domain: &str,
    jvm_roots: &[String],
) -> Result<BTreeMap<&'a str, &'a str>, &'static str> {
    let mut occupied: BTreeSet<String> = jvm_roots.iter().map(|name| name.to_ascii_lowercase()).collect();
    if occupied.len() != jvm_roots.len() {
        return Err("duplicate JVM command root");
    }
    let mut visible = BTreeMap::new();
    for (id, command) in commands {
        if !ancestor(&command.domain, domain) {
            continue;
        }
        for root in std::iter::once(&command.name).chain(&command.aliases) {
            if !occupied.insert(root.to_ascii_lowercase()) {
                return Err("visible command root/alias has multiple owners");
            }
            visible.insert(root.as_str(), id.as_str());
        }
    }
    Ok(visible)
}

fn ancestor(scope: &str, domain: &str) -> bool {
    scope.is_empty() || scope == domain || domain.strip_prefix(scope).is_some_and(|suffix| suffix.starts_with('/'))
}

fn query<'a>(functions: &'a BTreeMap<String, Function>, path: &str) -> Result<&'a Function, &'static str> {
    functions.get(path).filter(|function| function.kind == FunctionKind::Query).ok_or("unknown command query reference")
}

fn literal(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_lowercase()
        && value.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-'))
}

#[cfg(test)]
mod tests;
