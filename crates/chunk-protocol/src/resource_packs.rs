//! The 26.2 configuration packet that offers the client a resource pack.
use crate::commands::PlainText;
use crate::versions::v26_2::ADD_RESOURCE_PACK_ID;
use crate::{Encode, McString, Packet, Result, Uuid};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddResourcePack {
    pub uuid: Uuid,
    pub url: McString<32767>,
    /// The pack's lowercase hex SHA-1.
    pub hash: McString<40>,
    pub forced: bool,
    pub prompt: Option<PlainText>,
}
impl Packet for AddResourcePack {
    const ID: i32 = ADD_RESOURCE_PACK_ID;
    const STATE: crate::State = crate::State::Configuration;
    const DIRECTION: crate::Direction = crate::Direction::Clientbound;
}
impl Encode for AddResourcePack {
    fn encode(&self, output: &mut Vec<u8>) -> Result<()> {
        self.uuid.encode(output)?;
        self.url.encode(output)?;
        self.hash.encode(output)?;
        self.forced.encode(output)?;
        self.prompt.encode(output)
    }
}
