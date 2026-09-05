use chunk_protocol::{Encode, McString, Packet, Result, VarInt, versions::v26_1::END_BIOME_ID};

pub(super) const SPAWN: [f64; 3] = [8.0, 64.0, 8.0];

#[derive(Packet)]
#[packet(id = 0x31, state = Play, direction = Clientbound)]
pub(super) struct JoinLimbo;

impl Encode for JoinLimbo {
    fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        1_i32.encode(out)?; // Local player entity.
        false.encode(out)?;
        VarInt(1).encode(out)?;
        McString::<32767>::new("chunk:limbo")?.encode(out)?;
        for value in [1, 2, 2] {
            VarInt(value).encode(out)?;
        }
        for value in [false, true, false] {
            value.encode(out)?;
        }
        VarInt(2).encode(out)?; // Vanilla End dimension type.
        McString::<32767>::new("chunk:limbo")?.encode(out)?;
        0_i64.encode(out)?;
        3_u8.encode(out)?; // Spectator keeps the player floating in the void.
        255_u8.encode(out)?;
        for value in [false, false, false] {
            value.encode(out)?;
        } // Debug, flat, death location.
        VarInt(0).encode(out)?;
        VarInt(63).encode(out)?;
        false.encode(out)
    }
}

#[derive(Packet)]
#[packet(id = 0x72, state = Play, direction = Clientbound)]
pub(super) struct PreparingTitle;

impl Encode for PreparingTitle {
    fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let text = b"Preparing your server...";
        8_u8.encode(out)?; // Anonymous NBT string text component.
        u16::try_from(text.len()).expect("short title").encode(out)?;
        out.extend_from_slice(text);
        Ok(())
    }
}

/// An empty End column: sixteen air sections, with no block or sky light.
#[derive(Packet)]
#[packet(id = 0x2d, state = Play, direction = Clientbound)]
pub(super) struct LimboChunk {
    pub x: i32,
    pub z: i32,
}

impl Encode for LimboChunk {
    fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        self.x.encode(out)?;
        self.z.encode(out)?;
        VarInt(2).encode(out)?;
        for kind in [1, 4] {
            VarInt(kind).encode(out)?;
            VarInt(37).encode(out)?; // 256 empty height values, nine bits each.
            for _ in 0..37 {
                0_i64.encode(out)?;
            }
        }
        let mut sections = Vec::new();
        for _ in 0..16 {
            sections.extend_from_slice(&[0, 0, 0, 0]); // Block and fluid counts.
            sections.extend_from_slice(&[0, 0]); // Single-value air palette.
            0_u8.encode(&mut sections)?;
            VarInt(END_BIOME_ID).encode(&mut sections)?;
        }
        VarInt(i32::try_from(sections.len()).expect("bounded chunk sections")).encode(out)?;
        out.extend_from_slice(&sections);
        VarInt(0).encode(out)?; // No block entities.
        VarInt(0).encode(out)?; // No sky light arrays.
        VarInt(0).encode(out)?; // No block light arrays.
        for _ in 0..2 {
            VarInt(1).encode(out)?;
            ((1_i64 << 18) - 1).encode(out)?; // Empty sections, including both borders.
        }
        VarInt(0).encode(out)?;
        VarInt(0).encode(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chunk_protocol::Decode;

    #[test]
    fn end_columns_have_only_air_and_empty_light() {
        let mut bytes = Vec::new();
        LimboChunk { x: 0, z: 0 }.encode(&mut bytes).unwrap();
        let mut input = &bytes[8..];
        assert_eq!(VarInt::decode(&mut input).unwrap().0, 2);
        for kind in [1, 4] {
            assert_eq!(VarInt::decode(&mut input).unwrap().0, kind);
            assert_eq!(VarInt::decode(&mut input).unwrap().0, 37);
            for _ in 0..37 {
                assert_eq!(i64::decode(&mut input).unwrap(), 0);
            }
        }
        let length = usize::try_from(VarInt::decode(&mut input).unwrap().0).unwrap();
        let (mut sections, rest) = input.split_at(length);
        for _ in 0..16 {
            assert_eq!(&sections[..7], &[0; 7]);
            sections = &sections[7..];
            assert_eq!(VarInt::decode(&mut sections).unwrap().0, END_BIOME_ID);
        }
        assert!(sections.is_empty());
        input = rest;
        for _ in 0..3 {
            assert_eq!(VarInt::decode(&mut input).unwrap().0, 0);
        }
        for _ in 0..2 {
            assert_eq!(VarInt::decode(&mut input).unwrap().0, 1);
            assert_eq!(i64::decode(&mut input).unwrap(), (1 << 18) - 1);
        }
        for _ in 0..2 {
            assert_eq!(VarInt::decode(&mut input).unwrap().0, 0);
        }
        assert!(input.is_empty());
    }
}
