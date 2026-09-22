#![cfg(feature = "mc-26-2")]
use bytes::BytesMut;
use chunk_protocol::{
    Decode, Encode, McString, Packet, VarInt,
    commands::{
        ArgumentParser, CommandNode, CommandSuggestions, CommandTree, NodeKind, PlainText, PropertyKind, SignedCommand,
        SystemMessage,
    },
    decode_frame, decode_packet, encode_packet,
    versions::v26_2::{CommandSuggestionsRequest, UnsignedCommand, commands::PARSERS},
};

fn roundtrip<T: Packet + Encode + Decode + PartialEq + std::fmt::Debug>(value: &T) {
    let mut bytes = BytesMut::from(encode_packet(value).unwrap().as_slice());
    let frame = decode_frame(&mut bytes, 2 * 1024 * 1024).unwrap().unwrap();
    assert_eq!(&decode_packet::<T>(&frame).unwrap(), value);
    assert!(bytes.is_empty());
}
#[test]
fn every_pinned_parser_property_shape_roundtrips_with_cycles_and_restricted_nodes() {
    let mut tree = CommandTree::empty();
    for (id, _, kind) in PARSERS {
        let mut bytes = Vec::new();
        match kind {
            PropertyKind::None => {}
            PropertyKind::Numeric32 => {
                bytes.push(3);
                bytes.extend([0; 8]);
            }
            PropertyKind::Numeric64 => {
                bytes.push(3);
                bytes.extend([0; 16]);
            }
            PropertyKind::StringMode => bytes.extend([0x81, 0]), // Preserve validated noncanonical VarInt property bytes.
            PropertyKind::Entity => bytes.push(3),
            PropertyKind::ScoreHolder => bytes.push(1),
            PropertyKind::Time => bytes.extend([0; 4]),
            PropertyKind::Registry => McString::<256>::new("minecraft:block").unwrap().encode(&mut bytes).unwrap(),
        }
        let index = tree.nodes.len();
        let parser = ArgumentParser::new(*id, bytes.clone()).unwrap();
        assert_eq!(parser.properties(), bytes);
        let mut node = CommandNode::new(NodeKind::Argument {
            name: McString::new(format!("arg{id}")).unwrap(),
            parser,
            suggestions: Some(McString::new("minecraft:ask_server").unwrap()),
        });
        node.restricted = true;
        node.redirect = Some(0);
        node.children.push(index);
        tree.nodes.push(node);
        tree.nodes[0].children.push(index);
    }
    roundtrip(&tree);
    let mut broken = tree.clone();
    broken.nodes[0].children.push(tree.nodes.len());
    assert!(encode_packet(&broken).is_err());
    broken.root = 1;
    assert!(broken.validate().is_err());
    for (id, _, kind) in PARSERS {
        if *kind != PropertyKind::None {
            assert!(ArgumentParser::new(*id, vec![]).is_err());
        }
    }
    assert!(ArgumentParser::new(57, vec![]).is_err());
    assert!(ArgumentParser::new(3, vec![4]).is_err());
    assert!(ArgumentParser::new(5, vec![3]).is_err());
    assert!(ArgumentParser::new(6, vec![4]).is_err());
    assert!(ArgumentParser::new(31, vec![2]).is_err());
    assert!(CommandTree::decode(&mut &[1, 0x40, 0, 0][..]).is_err());
    assert!(CommandTree::decode(&mut &[1, 0, 1, 1, 0][..]).is_err());
}
#[test]
fn unsigned_and_signed_command_boundaries_reject_truncation_without_rewriting_signatures() {
    roundtrip(&UnsignedCommand { command: McString::new("travel '🎮 arena'").unwrap() });
    roundtrip(&CommandSuggestionsRequest { transaction_id: VarInt(7), text: McString::new("/travel ar").unwrap() });
    assert!(McString::<1024>::new("🎮".repeat(513)).is_err());
    let mut frame = Vec::new();
    VarInt(SignedCommand::ID).encode(&mut frame).unwrap();
    McString::<1024>::new("vanilla argument").unwrap().encode(&mut frame).unwrap();
    42_i64.encode(&mut frame).unwrap();
    43_i64.encode(&mut frame).unwrap();
    VarInt(1).encode(&mut frame).unwrap();
    McString::<64>::new("argument").unwrap().encode(&mut frame).unwrap();
    frame.extend([0xa5; 256]);
    VarInt(2).encode(&mut frame).unwrap();
    frame.extend([1, 2, 3, 4]);
    let original = frame.clone();
    let envelope = decode_packet::<SignedCommand>(&frame).unwrap();
    assert_eq!(envelope.command.as_str(), "vanilla argument");
    assert_eq!(envelope.signature_count, 1);
    assert_eq!(original, frame);
    assert!(decode_packet::<SignedCommand>(&frame[..frame.len() - 1]).is_err());
    frame.push(0);
    assert!(decode_packet::<SignedCommand>(&frame).is_err());
}
#[test]
fn suggestions_and_plain_text_nbt_are_bounded_and_unicode_safe() {
    roundtrip(&CommandSuggestions {
        transaction_id: 5,
        start: 7,
        length: 2,
        matches: vec![McString::new("\"🎮 arena\"").unwrap()],
    });
    assert!(encode_packet(&CommandSuggestions { transaction_id: 1, start: 1025, length: 1, matches: vec![] }).is_err());
    assert!(
        encode_packet(&CommandSuggestions {
            transaction_id: 1,
            start: 0,
            length: 0,
            matches: vec![McString::new("x").unwrap(); 65]
        })
        .is_err()
    );
    let mut nbt = Vec::new();
    PlainText::new("A\0🎮").unwrap().encode(&mut nbt).unwrap();
    assert_eq!(
        nbt,
        [10, 8, 0, 4, b't', b'e', b'x', b't', 0, 9, b'A', 0xc0, 0x80, 0xed, 0xa0, 0xbc, 0xed, 0xbe, 0xae, 0]
    );
    assert!(PlainText::new("a".repeat(4097)).is_err());
    let text = PlainText::new("{\"clickEvent\":\"untrusted text\"}").unwrap();
    let mut encoded = Vec::new();
    SystemMessage { text, overlay: false }.encode(&mut encoded).unwrap();
    assert_eq!(encoded[0], 10);
    assert_eq!(*encoded.last().unwrap(), 0);
}
