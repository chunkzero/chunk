use crate::versions::v26_2::commands::{ACTION_BAR_ID, SUBTITLE_TEXT_ID, SYSTEM_MESSAGE_ID, TITLE_TEXT_ID};
use crate::{Encode, Error, McString, Packet, Result};

/// Plain text only: encoded as a network NBT component with one `text` field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlainText(McString<4096>);
impl PlainText {
    /// # Errors
    /// Rejects text longer than 4096 UTF-16 code units.
    pub fn new(text: impl Into<String>) -> Result<Self> {
        Ok(Self(McString::new(text)?))
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}
impl Encode for PlainText {
    fn encode(&self, output: &mut Vec<u8>) -> Result<()> {
        output.extend_from_slice(&[10, 8, 0, 4, b't', b'e', b'x', b't']);
        let mut bytes = Vec::new();
        // NBT uses Java modified UTF-8: NUL is two bytes, supplementary characters use surrogate pairs.
        for unit in self.0.as_str().encode_utf16() {
            if unit != 0 && unit <= 0x7f {
                bytes.push(u8::try_from(unit).map_err(|_| Error::InvalidUtf8)?);
            } else if unit <= 0x7ff {
                bytes.push(0xc0 | u8::try_from(unit >> 6).map_err(|_| Error::InvalidUtf8)?);
                bytes.push(0x80 | u8::try_from(unit & 0x3f).map_err(|_| Error::InvalidUtf8)?);
            } else {
                bytes.push(0xe0 | u8::try_from(unit >> 12).map_err(|_| Error::InvalidUtf8)?);
                bytes.push(0x80 | u8::try_from((unit >> 6) & 0x3f).map_err(|_| Error::InvalidUtf8)?);
                bytes.push(0x80 | u8::try_from(unit & 0x3f).map_err(|_| Error::InvalidUtf8)?);
            }
        }
        u16::try_from(bytes.len()).map_err(|_| Error::StringTooLong)?.encode(output)?;
        output.extend_from_slice(&bytes);
        output.push(0);
        Ok(())
    }
}

macro_rules! text_packet {
    ($name:ident, $id:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name {
            pub text: PlainText,
        }
        impl Packet for $name {
            const ID: i32 = $id;
            const STATE: crate::State = crate::State::Play;
            const DIRECTION: crate::Direction = crate::Direction::Clientbound;
        }
        impl Encode for $name {
            fn encode(&self, output: &mut Vec<u8>) -> Result<()> {
                self.text.encode(output)
            }
        }
    };
}
text_packet!(ActionBar, ACTION_BAR_ID);
text_packet!(TitleText, TITLE_TEXT_ID);
text_packet!(SubtitleText, SUBTITLE_TEXT_ID);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemMessage {
    pub text: PlainText,
    pub overlay: bool,
}
impl Packet for SystemMessage {
    const ID: i32 = SYSTEM_MESSAGE_ID;
    const STATE: crate::State = crate::State::Play;
    const DIRECTION: crate::Direction = crate::Direction::Clientbound;
}
impl Encode for SystemMessage {
    fn encode(&self, output: &mut Vec<u8>) -> Result<()> {
        self.text.encode(output)?;
        self.overlay.encode(output)
    }
}
