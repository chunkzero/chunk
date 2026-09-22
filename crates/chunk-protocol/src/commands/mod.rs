//! Bounded 26.2 command and plain text packets. Signed packets are inspected, never rewritten.
mod text;
mod tree;
pub use text::{ActionBar, PlainText, SubtitleText, SystemMessage, TitleText};
pub use tree::{ArgumentParser, CommandNode, CommandTree, MAX_NODES, NodeKind, PropertyKind, StringMode};

use crate::versions::v26_2::commands::{COMMAND_SUGGESTIONS_ID, SIGNED_COMMAND_ID};
use crate::{Decode, Encode, Error, McString, Packet, Result, VarInt};

/// Validated signed-command envelope. Forward the original frame for JVM-owned roots.
/// No encoder is provided: signatures and acknowledgment bytes must not be rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedCommand {
    pub command: McString<1024>,
    pub signature_count: usize,
}
impl Packet for SignedCommand {
    const ID: i32 = SIGNED_COMMAND_ID;
    const STATE: crate::State = crate::State::Play;
    const DIRECTION: crate::Direction = crate::Direction::Serverbound;
}
impl Decode for SignedCommand {
    fn decode(input: &mut &[u8]) -> Result<Self> {
        if input.len() > 32 * 1024 {
            return Err(Error::InvalidFrameLength);
        }
        let command = McString::decode(input)?;
        i64::decode(input)?; // Timestamp.
        i64::decode(input)?; // Salt.
        let signature_count = count(input, 64)?;
        for _ in 0..signature_count {
            McString::<64>::decode(input)?;
            take(input, 256)?;
        }
        if VarInt::decode(input)?.0 < 0 {
            return Err(Error::InvalidCommand);
        }
        take(input, 3)?; // Last-seen acknowledgment bitset.
        u8::decode(input)?; // Checksum.
        Ok(Self { command, signature_count })
    }
}

/// Proxy-owned suggestions have no tooltips. JVM responses remain untouched by the relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSuggestions {
    pub transaction_id: i32,
    pub start: u32,
    pub length: u32,
    pub matches: Vec<McString<1024>>,
}
impl CommandSuggestions {
    fn validate(&self) -> Result<()> {
        if self.matches.len() > 64 || self.start.checked_add(self.length).is_none_or(|end| end > 1025) {
            return Err(Error::InvalidCommand);
        }
        Ok(())
    }
}
impl Packet for CommandSuggestions {
    const ID: i32 = COMMAND_SUGGESTIONS_ID;
    const STATE: crate::State = crate::State::Play;
    const DIRECTION: crate::Direction = crate::Direction::Clientbound;
}
impl Encode for CommandSuggestions {
    fn encode(&self, output: &mut Vec<u8>) -> Result<()> {
        self.validate()?;
        VarInt(self.transaction_id).encode(output)?;
        write_count(self.start as usize, output)?;
        write_count(self.length as usize, output)?;
        write_count(self.matches.len(), output)?;
        for item in &self.matches {
            item.encode(output)?;
            false.encode(output)?;
        }
        Ok(())
    }
}
impl Decode for CommandSuggestions {
    fn decode(input: &mut &[u8]) -> Result<Self> {
        if input.len() > 256 * 1024 {
            return Err(Error::InvalidFrameLength);
        }
        let transaction_id = VarInt::decode(input)?.0;
        let start = u32::try_from(VarInt::decode(input)?.0).map_err(|_| Error::InvalidCommand)?;
        let length = u32::try_from(VarInt::decode(input)?.0).map_err(|_| Error::InvalidCommand)?;
        let size = count(input, 64)?;
        let mut matches = Vec::with_capacity(size);
        for _ in 0..size {
            matches.push(McString::decode(input)?);
            if bool::decode(input)? {
                return Err(Error::InvalidCommand);
            }
        }
        let value = Self { transaction_id, start, length, matches };
        value.validate()?;
        Ok(value)
    }
}

fn count(input: &mut &[u8], limit: usize) -> Result<usize> {
    let count = usize::try_from(VarInt::decode(input)?.0).map_err(|_| Error::CollectionTooLong)?;
    if count > limit {
        return Err(Error::CollectionTooLong);
    }
    Ok(count)
}
fn write_count(count: usize, output: &mut Vec<u8>) -> Result<()> {
    VarInt(i32::try_from(count).map_err(|_| Error::CollectionTooLong)?).encode(output)
}
fn take<'a>(input: &mut &'a [u8], length: usize) -> Result<&'a [u8]> {
    let (value, rest) = input.split_at_checked(length).ok_or(Error::Incomplete)?;
    *input = rest;
    Ok(value)
}
