//! Pure command ownership, publication and suggestion planning. No backend calls or authorization.
use std::collections::{BTreeMap, BTreeSet};

use chunk_contract::{Command, CommandParser, ParsedCommand, visible_commands};
use chunk_protocol::{
    McString,
    commands::{ArgumentParser, CommandNode, CommandTree, NodeKind, StringMode},
};

mod suggestions;
pub use suggestions::SuggestionPlan;
#[cfg(test)]
mod tests;

type Result<T> = std::result::Result<T, &'static str>;

/// All inherited owners are captured before permission filtering. A denied root remains owned.
#[derive(Debug, Clone)]
pub struct CommandTreeCatalog {
    jvm: CommandTree,
    commands: BTreeMap<String, Command>,
    owners: BTreeMap<String, String>,
}
impl CommandTreeCatalog {
    /// `commands` must come from a validated deployment. `domain` selects inherited descriptors.
    /// # Errors
    /// Rejects invalid JVM trees, root/alias collisions and excessive merged trees.
    pub fn new(jvm: CommandTree, commands: &BTreeMap<String, Command>, domain: &str) -> Result<Self> {
        jvm.validate().map_err(|_| "invalid JVM command tree")?;
        let roots = jvm.nodes[jvm.root]
            .children
            .iter()
            .map(|index| match &jvm.nodes[*index].kind {
                NodeKind::Literal { name } => Ok(name.as_str().to_owned()),
                _ => Err("JVM command root children must be literals"),
            })
            .collect::<Result<Vec<_>>>()?;
        let owners = visible_commands(commands, domain, &roots)?
            .into_iter()
            .map(|(root, id)| (root.to_owned(), id.to_owned()))
            .collect::<BTreeMap<_, _>>();
        let selected: BTreeSet<_> = owners.values().collect();
        let commands = commands
            .iter()
            .filter(|(id, _)| selected.contains(id))
            .map(|(id, command)| (id.clone(), command.clone()))
            .collect();
        let catalog = Self { jvm, commands, owners };
        catalog.merge(|_, _| true)?;
        Ok(catalog)
    }

    /// Classifies ownership even when the command is hidden or its remaining arguments are invalid.
    #[must_use]
    pub fn owner(&self, input: &str) -> Option<(&str, &Command)> {
        let body = input.strip_prefix('/').unwrap_or(input);
        let root = body.split(' ').next()?;
        let id = self.owners.get(root)?;
        Some((id, &self.commands[id]))
    }

    /// # Errors
    /// Rejects unowned inputs and invalid arguments. The caller must authorize this owner separately.
    pub fn parse(&self, input: &str) -> Result<(&str, ParsedCommand)> {
        let (id, command) = self.owner(input).ok_or("unowned command")?;
        Ok((id, command.parse(input)?))
    }

    /// Permission results affect publication only, never ownership. Existing JVM indices stay fixed.
    /// # Errors
    /// Rejects conflicting child names and bounded tree overflow.
    pub fn merge(&self, visible: impl Fn(&str, &Command) -> bool) -> Result<CommandTree> {
        let mut tree = self.jvm.clone();
        for (id, command) in &self.commands {
            if !visible(id, command) {
                continue;
            }
            let root = append(&mut tree, CommandNode::new(literal(&command.name)?))?;
            tree.nodes[tree.root].children.push(root);
            for route in &command.routes {
                let mut parent = root;
                for name in &route.literals {
                    let existing = tree.nodes[parent]
                        .children
                        .iter()
                        .copied()
                        .find(|index| tree.nodes[*index].name() == Some(name));
                    parent = if let Some(index) = existing {
                        if !matches!(tree.nodes[index].kind, NodeKind::Literal { .. }) {
                            return Err("conflicting command child name");
                        }
                        index
                    } else {
                        child(&mut tree, parent, literal(name)?)?
                    };
                }
                for argument in &route.arguments {
                    if tree.nodes[parent].children.iter().any(|index| tree.nodes[*index].name() == Some(&argument.name))
                    {
                        return Err("conflicting command child name");
                    }
                    let parser = match argument.parser {
                        CommandParser::Boolean => ArgumentParser::boolean(),
                        CommandParser::Integer => ArgumentParser::integer(argument.min, argument.max),
                        CommandParser::Word => ArgumentParser::string(StringMode::Word),
                        CommandParser::String => ArgumentParser::string(StringMode::String),
                        CommandParser::Greedy => ArgumentParser::string(StringMode::Greedy),
                    }
                    .map_err(|_| "invalid command parser")?;
                    let suggestions = argument
                        .suggestions
                        .as_ref()
                        .map(|_| McString::new("minecraft:ask_server"))
                        .transpose()
                        .map_err(|_| "invalid suggestions identifier")?;
                    parent = child(
                        &mut tree,
                        parent,
                        NodeKind::Argument {
                            name: McString::new(argument.name.clone()).map_err(|_| "invalid argument name")?,
                            parser,
                            suggestions,
                        },
                    )?;
                }
                tree.nodes[parent].executable = true;
            }
            for alias in &command.aliases {
                let mut node = CommandNode::new(literal(alias)?);
                node.redirect = Some(root);
                node.executable = tree.nodes[root].executable;
                let index = append(&mut tree, node)?;
                tree.nodes[tree.root].children.push(index);
            }
        }
        tree.validate().map_err(|_| "merged command tree exceeds limits")?;
        let mut encoded = Vec::new();
        chunk_protocol::Encode::encode(&tree, &mut encoded).map_err(|_| "merged command tree exceeds byte limit")?;
        Ok(tree)
    }
}
fn literal(name: &str) -> Result<NodeKind> {
    Ok(NodeKind::Literal { name: McString::new(name.to_owned()).map_err(|_| "invalid literal name")? })
}
fn append(tree: &mut CommandTree, node: CommandNode) -> Result<usize> {
    if tree.nodes.len() >= 8192 {
        return Err("merged command node limit");
    }
    let index = tree.nodes.len();
    tree.nodes.push(node);
    Ok(index)
}
fn child(tree: &mut CommandTree, parent: usize, kind: NodeKind) -> Result<usize> {
    let index = append(tree, CommandNode::new(kind))?;
    tree.nodes[parent].children.push(index);
    Ok(index)
}
