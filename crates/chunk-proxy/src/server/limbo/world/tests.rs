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
