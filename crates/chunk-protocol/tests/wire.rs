use chunk_protocol::{Decode, Direction, Encode, Error, McString, Packet, State, VarInt, decode_packet, encode_packet};

#[test]
fn varints_match_wire_vectors_and_reject_overflow() {
    for (value, expected) in [
        (0, vec![0]),
        (127, vec![127]),
        (128, vec![0x80, 1]),
        (i32::MAX, vec![0xff, 0xff, 0xff, 0xff, 7]),
        (-1, vec![0xff, 0xff, 0xff, 0xff, 0x0f]),
        (i32::MIN, vec![0x80, 0x80, 0x80, 0x80, 8]),
    ] {
        let mut encoded = vec![];
        VarInt(value).encode(&mut encoded).unwrap();
        assert_eq!(encoded, expected);
        assert_eq!(VarInt::decode(&mut expected.as_slice()).unwrap().0, value);
    }
    for bytes in [&[0x80, 0x80, 0x80, 0x80, 0x10][..], &[0xff; 5]] {
        assert_eq!(VarInt::decode(&mut &bytes[..]), Err(Error::InvalidVarInt));
    }
    assert_eq!(VarInt::decode(&mut &[0x80][..]), Err(Error::Incomplete));
}

#[test]
#[cfg(feature = "mc-26-1")]
fn handshake_matches_wire_and_framing_preserves_partial_input() {
    use bytes::BytesMut;
    use chunk_protocol::{decode_frame, versions::v26_1::Handshake};

    // Protocol 47, localhost:25565, status intention. Followed by status request.
    let wire = b"\x0f\x00\x2f\x09localhost\x63\xdd\x01";
    for split in 0..wire.len() {
        let mut buffer = BytesMut::from(&wire[..split]);
        let before = buffer.clone();
        assert!(decode_frame(&mut buffer, 4096).unwrap().is_none());
        assert_eq!(buffer, before);
        buffer.extend_from_slice(&wire[split..]);
        buffer.extend_from_slice(&[1, 0]);
        let frame = decode_frame(&mut buffer, 4096).unwrap().unwrap();
        let packet = decode_packet::<Handshake>(&frame).unwrap();
        assert_eq!(packet.protocol_version, VarInt(47));
        assert_eq!(packet.server_address.as_str(), "localhost");
        assert_eq!(packet.server_port, 25565);
        assert_eq!(packet.next_state, VarInt(1));
        assert_eq!(encode_packet(&packet).unwrap(), wire);
        assert_eq!(&buffer[..], &[1, 0]);
    }
    for wire in [&[0][..], &[0x80, 0x80, 0x80], &[0x81, 0x20]] {
        assert_eq!(
            decode_frame(&mut BytesMut::from(wire), 4096),
            Err(Error::InvalidFrameLength)
        );
    }
}

#[test]
fn strings_enforce_java_length_and_valid_utf8() {
    assert!(McString::<1>::new("😀").is_err());
    let value = McString::<2>::new("😀").unwrap();
    let mut bytes = vec![];
    value.encode(&mut bytes).unwrap();
    assert_eq!(bytes, [4, 0xf0, 0x9f, 0x98, 0x80]);
    assert_eq!(McString::<2>::decode(&mut bytes.as_slice()).unwrap(), value);
    assert_eq!(McString::<1>::decode(&mut bytes.as_slice()), Err(Error::StringTooLong));
    assert_eq!(McString::<1>::decode(&mut &[1, 0xff][..]), Err(Error::InvalidUtf8));
    assert_eq!(McString::<1>::decode(&mut &[2, b'a'][..]), Err(Error::Incomplete));
}

#[derive(Debug, PartialEq, Encode, Decode, Packet)]
#[packet(id = 0x02, state = Login, direction = Clientbound)]
struct Generic<T>(T, u16);

#[test]
fn derived_generic_packet_checks_id_and_consumes_entire_payload() {
    let packet = Generic(VarInt(128), 25565);
    assert_eq!(encode_packet(&packet).unwrap(), [5, 2, 0x80, 1, 0x63, 0xdd]);
    assert_eq!(Generic::<VarInt>::STATE, State::Login);
    assert_eq!(Generic::<VarInt>::DIRECTION, Direction::Clientbound);
    assert_eq!(
        decode_packet::<Generic<VarInt>>(&[2, 0x80, 1, 0x63, 0xdd]).unwrap(),
        packet
    );
    assert_eq!(
        decode_packet::<Generic<VarInt>>(&[1, 0, 0, 0]),
        Err(Error::UnexpectedPacket)
    );
    assert_eq!(
        decode_packet::<Generic<VarInt>>(&[2, 0, 0, 0, 1]),
        Err(Error::TrailingBytes)
    );
}

#[test]
fn bounded_collections_and_optional_values_reject_invalid_input() {
    use chunk_protocol::{BoundedArray, ByteArray, RemainingBytes, Uuid};

    let bytes = ByteArray::<2>::new(vec![0xab, 0xcd]).unwrap();
    let mut encoded = vec![];
    bytes.encode(&mut encoded).unwrap();
    assert_eq!(encoded, [2, 0xab, 0xcd]);
    assert_eq!(ByteArray::<2>::decode(&mut encoded.as_slice()).unwrap(), bytes);
    assert_eq!(ByteArray::<2>::new(vec![0; 3]), Err(Error::CollectionTooLong));
    assert_eq!(ByteArray::<2>::decode(&mut &[3][..]), Err(Error::CollectionTooLong));
    assert_eq!(ByteArray::<2>::decode(&mut &[2, 0][..]), Err(Error::Incomplete));
    assert_eq!(
        BoundedArray::<VarInt, 2>::decode(&mut &[0xff, 0xff, 0xff, 0xff, 0x0f][..]),
        Err(Error::CollectionTooLong)
    );
    assert_eq!(Option::<VarInt>::decode(&mut &[2][..]), Err(Error::InvalidBoolean));
    assert_eq!(Option::<VarInt>::decode(&mut &[1][..]), Err(Error::Incomplete));
    assert_eq!(Uuid::decode(&mut &[0; 15][..]), Err(Error::Incomplete));
    assert_eq!(
        RemainingBytes::<2>::decode(&mut &[0; 3][..]),
        Err(Error::CollectionTooLong)
    );
}
