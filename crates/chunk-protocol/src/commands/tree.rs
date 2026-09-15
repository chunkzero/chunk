use super::{count, take, write_count};
use crate::versions::v26_1::commands::{COMMAND_TREE_ID, PARSERS};
use crate::{Decode, Encode, Error, McString, Packet, Result, VarInt};

pub const MAX_NODES: usize = 8192;
const MAX_EDGES: usize = 32768;
const MAX_BYTES: usize = 1024 * 1024;

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyKind {
    None,
    Numeric32,
    Numeric64,
    StringMode,
    Entity,
    ScoreHolder,
    Time,
    Registry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringMode {
    Word,
    String,
    Greedy,
}

/// Known parser identity plus its validated original property bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgumentParser {
    id: i32,
    properties: Vec<u8>,
}
impl ArgumentParser {
    /// # Errors
    /// Rejects unknown parser IDs, malformed properties and trailing property bytes.
    pub fn new(id: i32, properties: Vec<u8>) -> Result<Self> {
        let mut input = properties.as_slice();
        properties_end(id, &mut input)?;
        if !input.is_empty() {
            return Err(Error::TrailingBytes);
        }
        Ok(Self { id, properties })
    }
    #[must_use]
    pub fn id(&self) -> i32 {
        self.id
    }
    #[must_use]
    pub fn properties(&self) -> &[u8] {
        &self.properties
    }
    /// # Errors
    /// Fails if the pinned parser mapping is unavailable.
    pub fn boolean() -> Result<Self> {
        Self::named("brigadier:bool", Vec::new())
    }
    /// # Errors
    /// Rejects a minimum greater than the maximum.
    pub fn integer(min: Option<i32>, max: Option<i32>) -> Result<Self> {
        if min.zip(max).is_some_and(|(min, max)| min > max) {
            return Err(Error::InvalidCommand);
        }
        let mut properties = vec![u8::from(min.is_some()) | (u8::from(max.is_some()) << 1)];
        if let Some(min) = min {
            min.encode(&mut properties)?;
        }
        if let Some(max) = max {
            max.encode(&mut properties)?;
        }
        Self::named("brigadier:integer", properties)
    }
    /// # Errors
    /// Fails if the pinned parser mapping is unavailable.
    pub fn string(mode: StringMode) -> Result<Self> {
        Self::named(
            "brigadier:string",
            vec![match mode {
                StringMode::Word => 0,
                StringMode::String => 1,
                StringMode::Greedy => 2,
            }],
        )
    }
    fn named(name: &str, properties: Vec<u8>) -> Result<Self> {
        let id = PARSERS.iter().find(|(_, candidate, _)| *candidate == name).ok_or(Error::InvalidCommand)?.0;
        Self::new(id, properties)
    }
    fn read(input: &mut &[u8]) -> Result<Self> {
        let id = VarInt::decode(input)?.0;
        let original = *input;
        properties_end(id, input)?;
        Ok(Self { id, properties: original[..original.len() - input.len()].to_vec() })
    }
    fn write(&self, output: &mut Vec<u8>) -> Result<()> {
        VarInt(self.id).encode(output)?;
        output.extend_from_slice(&self.properties);
        Ok(())
    }
}
fn properties_end(id: i32, input: &mut &[u8]) -> Result<()> {
    let kind = PARSERS.iter().find(|(candidate, _, _)| *candidate == id).ok_or(Error::InvalidCommand)?.2;
    match kind {
        PropertyKind::None => {}
        PropertyKind::Numeric32 | PropertyKind::Numeric64 => {
            let flags = u8::decode(input)?;
            if flags & !3 != 0 {
                return Err(Error::InvalidCommand);
            }
            let size = if kind == PropertyKind::Numeric32 { 4 } else { 8 };
            if flags & 1 != 0 {
                take(input, size)?;
            }
            if flags & 2 != 0 {
                take(input, size)?;
            }
        }
        PropertyKind::StringMode => {
            if !(0..=2).contains(&VarInt::decode(input)?.0) {
                return Err(Error::InvalidCommand);
            }
        }
        PropertyKind::Entity | PropertyKind::ScoreHolder => {
            let mask = if kind == PropertyKind::Entity { 3 } else { 1 };
            if u8::decode(input)? & !mask != 0 {
                return Err(Error::InvalidCommand);
            }
        }
        PropertyKind::Time => {
            i32::decode(input)?;
        }
        PropertyKind::Registry => {
            McString::<256>::decode(input)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    Root,
    Literal { name: McString<256> },
    Argument { name: McString<256>, parser: ArgumentParser, suggestions: Option<McString<256>> },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandNode {
    pub kind: NodeKind,
    pub executable: bool,
    pub restricted: bool,
    pub children: Vec<usize>,
    pub redirect: Option<usize>,
}
impl CommandNode {
    #[must_use]
    pub fn new(kind: NodeKind) -> Self {
        Self { kind, executable: false, restricted: false, children: Vec::new(), redirect: None }
    }
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match &self.kind {
            NodeKind::Root => None,
            NodeKind::Literal { name } | NodeKind::Argument { name, .. } => Some(name.as_str()),
        }
    }
    fn read(input: &mut &[u8], edges: &mut usize) -> Result<Self> {
        let flags = u8::decode(input)?;
        if flags & 0xc0 != 0 || flags & 3 == 3 {
            return Err(Error::InvalidCommand);
        }
        let children_count = count(input, MAX_NODES)?;
        *edges += children_count;
        if *edges > MAX_EDGES {
            return Err(Error::CollectionTooLong);
        }
        let children = (0..children_count).map(|_| count(input, MAX_NODES - 1)).collect::<Result<Vec<_>>>()?;
        let redirect = if flags & 8 != 0 { Some(count(input, MAX_NODES - 1)?) } else { None };
        let kind = match flags & 3 {
            0 => NodeKind::Root,
            1 => NodeKind::Literal { name: McString::decode(input)? },
            2 => NodeKind::Argument {
                name: McString::decode(input)?,
                parser: ArgumentParser::read(input)?,
                suggestions: if flags & 16 != 0 { Some(McString::decode(input)?) } else { None },
            },
            _ => return Err(Error::InvalidCommand),
        };
        if flags & 16 != 0 && !matches!(kind, NodeKind::Argument { .. }) {
            return Err(Error::InvalidCommand);
        }
        Ok(Self { kind, children, redirect, executable: flags & 4 != 0, restricted: flags & 32 != 0 })
    }
    fn write(&self, output: &mut Vec<u8>) -> Result<()> {
        let kind = match &self.kind {
            NodeKind::Root => 0,
            NodeKind::Literal { .. } => 1,
            NodeKind::Argument { .. } => 2,
        };
        let suggestions = matches!(&self.kind, NodeKind::Argument { suggestions: Some(_), .. });
        let flags = kind
            | (u8::from(self.executable) << 2)
            | (u8::from(self.redirect.is_some()) << 3)
            | (u8::from(suggestions) << 4)
            | (u8::from(self.restricted) << 5);
        flags.encode(output)?;
        write_count(self.children.len(), output)?;
        for child in &self.children {
            write_count(*child, output)?;
        }
        if let Some(redirect) = self.redirect {
            write_count(redirect, output)?;
        }
        match &self.kind {
            NodeKind::Root => {}
            NodeKind::Literal { name } => name.encode(output)?,
            NodeKind::Argument { name, parser, suggestions } => {
                name.encode(output)?;
                parser.write(output)?;
                if let Some(suggestions) = suggestions {
                    suggestions.encode(output)?;
                }
            }
        }
        Ok(())
    }
}

/// Node indices are preserved. Redirects and child cycles are legal; all indices must exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandTree {
    pub nodes: Vec<CommandNode>,
    pub root: usize,
}
impl CommandTree {
    #[must_use]
    pub fn empty() -> Self {
        Self { nodes: vec![CommandNode::new(NodeKind::Root)], root: 0 }
    }
    /// # Errors
    /// Rejects excessive graphs, invalid indices and a non-root root index.
    pub fn validate(&self) -> Result<()> {
        if self.nodes.is_empty()
            || self.nodes.len() > MAX_NODES
            || !self.nodes.get(self.root).is_some_and(|node| matches!(node.kind, NodeKind::Root))
        {
            return Err(Error::InvalidCommand);
        }
        let mut edges = 0;
        for node in &self.nodes {
            edges += node.children.len();
            if node.children.len() > MAX_NODES
                || edges > MAX_EDGES
                || node.children.iter().chain(node.redirect.iter()).any(|index| *index >= self.nodes.len())
            {
                return Err(Error::InvalidCommand);
            }
        }
        Ok(())
    }
}
impl Packet for CommandTree {
    const ID: i32 = COMMAND_TREE_ID;
    const STATE: crate::State = crate::State::Play;
    const DIRECTION: crate::Direction = crate::Direction::Clientbound;
}
impl Decode for CommandTree {
    fn decode(input: &mut &[u8]) -> Result<Self> {
        if input.len() > MAX_BYTES {
            return Err(Error::InvalidFrameLength);
        }
        let size = count(input, MAX_NODES)?;
        let mut edges = 0;
        let nodes = (0..size).map(|_| CommandNode::read(input, &mut edges)).collect::<Result<Vec<_>>>()?;
        let root = count(input, MAX_NODES - 1)?;
        let tree = Self { nodes, root };
        tree.validate()?;
        Ok(tree)
    }
}
impl Encode for CommandTree {
    fn encode(&self, output: &mut Vec<u8>) -> Result<()> {
        self.validate()?;
        let start = output.len();
        write_count(self.nodes.len(), output)?;
        for node in &self.nodes {
            node.write(output)?;
            if output.len() - start > MAX_BYTES {
                return Err(Error::InvalidFrameLength);
            }
        }
        write_count(self.root, output)?;
        if output.len() - start > MAX_BYTES {
            return Err(Error::InvalidFrameLength);
        }
        Ok(())
    }
}
