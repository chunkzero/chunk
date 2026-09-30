//! Packets the bot needs that chunk-protocol doesn't generate, by their 26.2 IDs, and disconnect reason text.

pub const CONFIGURATION_DISCONNECT: i32 = 0x02;
pub const CODE_OF_CONDUCT: i32 = 0x13;
pub const ACCEPT_CODE_OF_CONDUCT: i32 = 0x09;
pub const PLAY_DISCONNECT: i32 = 0x20;
/// Clientbound play `ping` (an `i32`), answered by serverbound `pong`.
pub const PLAY_PING: i32 = 0x3d;
pub const PLAY_PONG: i32 = 0x2d;

/// The readable text of a chat component in anonymous network NBT, best effort.
pub fn component_text(mut input: &[u8]) -> String {
    let mut text = String::new();
    if let Some((&tag, rest)) = input.split_first() {
        input = rest;
        let _ = walk(tag, &mut input, true, &mut text, 0);
    }
    text
}

fn take<'a>(input: &mut &'a [u8], count: usize) -> Option<&'a [u8]> {
    let (head, rest) = input.split_at_checked(count)?;
    *input = rest;
    Some(head)
}

fn length(input: &mut &[u8]) -> Option<usize> {
    usize::try_from(i32::from_be_bytes(take(input, 4)?.try_into().ok()?)).ok()
}

fn string<'a>(input: &mut &'a [u8]) -> Option<&'a [u8]> {
    let size = u16::from_be_bytes(take(input, 2)?.try_into().ok()?);
    take(input, usize::from(size))
}

/// Skips one NBT payload of `tag`, appending its strings to `text` when `keep`. None on malformed input.
fn walk(tag: u8, input: &mut &[u8], keep: bool, text: &mut String, depth: u8) -> Option<()> {
    let size = match tag {
        1 => 1,
        2 => 2,
        3 | 5 => 4,
        4 | 6 => 8,
        7 => length(input)?,
        11 => length(input)? * 4,
        12 => length(input)? * 8,
        8 => {
            let value = string(input)?;
            if keep {
                text.push_str(&String::from_utf8_lossy(value));
            }
            return Some(());
        }
        9 if depth < 16 => {
            let kind = take(input, 1)?[0];
            for _ in 0..length(input)? {
                walk(kind, input, keep, text, depth + 1)?;
            }
            return Some(());
        }
        10 if depth < 16 => loop {
            let kind = take(input, 1)?[0];
            if kind == 0 {
                return Some(());
            }
            let name = string(input)?;
            let keep = keep && matches!(name, b"text" | b"translate" | b"extra" | b"with" | b"");
            walk(kind, input, keep, text, depth + 1)?;
        },
        _ => return None,
    };
    take(input, size).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hard_coded_ids_match_the_dataset() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../chunk-protocol/data/26.2/protocol.json");
        let protocol: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let id = |state: &str, direction: &str, name: &str| {
            let mappings = &protocol[state][direction]["types"]["packet"][1][0]["type"][1]["mappings"];
            let (key, _) = mappings.as_object().unwrap().iter().find(|(_, value)| *value == name).unwrap();
            i32::from_str_radix(key.trim_start_matches("0x"), 16).unwrap()
        };
        assert_eq!(id("configuration", "toClient", "disconnect"), CONFIGURATION_DISCONNECT);
        assert_eq!(id("configuration", "toClient", "code_of_conduct"), CODE_OF_CONDUCT);
        assert_eq!(id("configuration", "toServer", "accept_code_of_conduct"), ACCEPT_CODE_OF_CONDUCT);
        assert_eq!(id("play", "toClient", "kick_disconnect"), PLAY_DISCONNECT);
        assert_eq!(id("play", "toClient", "ping"), PLAY_PING);
        assert_eq!(id("play", "toServer", "pong"), PLAY_PONG);
    }

    #[test]
    fn reads_plain_and_compound_reasons() {
        assert_eq!(component_text(b"\x08\x00\x09Timed out"), "Timed out");
        // {"color": "red", "text": "Full", "extra": ["!"]}
        let compound = b"\x0a\x08\x00\x05color\x00\x03red\x08\x00\x04text\x00\x04Full\x09\x00\x05extra\x08\x00\x00\x00\x01\x00\x01!\x00";
        assert_eq!(component_text(compound), "Full!");
    }
}
