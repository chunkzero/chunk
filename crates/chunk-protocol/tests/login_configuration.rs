#![cfg(feature = "mc-26-1")]

use chunk_protocol::{
    BoundedArray, ByteArray, Decode, Direction, Encode, Error, McString, Packet, State, Uuid, VarInt, decode_packet,
    encode_packet, versions::v26_1::*,
};

fn check_wire<P: Packet + Encode + Decode + std::fmt::Debug + PartialEq>(packet: &P, body: &[u8]) {
    let mut expected = Vec::new();
    VarInt(i32::try_from(body.len()).unwrap()).encode(&mut expected).unwrap();
    expected.extend_from_slice(body);
    assert_eq!(encode_packet(packet).unwrap(), expected);
    assert_eq!(&decode_packet::<P>(body).unwrap(), packet);
    for end in 0..body.len() {
        assert!(decode_packet::<P>(&body[..end]).is_err(), "accepted prefix {end}");
    }
}

#[test]
fn login_start_and_encryption_match_wire() {
    check_wire(
        &LoginStart {
            username: McString::new("Alex").unwrap(),
            player_uuid: Uuid([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]),
        },
        b"\x00\x04Alex\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f",
    );
    check_wire(
        &EncryptionRequest {
            server_id: McString::new("").unwrap(),
            public_key: ByteArray::new(vec![0x30, 0x01]).unwrap(),
            verify_token: ByteArray::new(vec![1, 2, 3, 4]).unwrap(),
            should_authenticate: true,
        },
        &[1, 0, 2, 0x30, 1, 4, 1, 2, 3, 4, 1],
    );
    check_wire(
        &EncryptionResponse {
            shared_secret: ByteArray::new(vec![0xaa, 0xbb]).unwrap(),
            verify_token: ByteArray::new(vec![0xcc]).unwrap(),
        },
        &[1, 2, 0xaa, 0xbb, 1, 0xcc],
    );
    check_wire(&SetCompression { threshold: VarInt(256) }, &[3, 0x80, 2]);
    check_wire(&LoginAcknowledged, &[3]);
}

#[test]
fn login_success_preserves_signed_and_unsigned_profile_properties() {
    let packet = LoginSuccess {
        uuid: Uuid([0x12; 16]),
        username: McString::new("Alex").unwrap(),
        properties: BoundedArray::new(vec![
            LoginSuccessPropertiesEntry {
                name: McString::new("textures").unwrap(),
                value: McString::new("abc").unwrap(),
                signature: Some(McString::new("sig").unwrap()),
            },
            LoginSuccessPropertiesEntry {
                name: McString::new("other").unwrap(),
                value: McString::new("").unwrap(),
                signature: None,
            },
        ])
        .unwrap(),
    };
    let mut body = vec![2];
    body.extend_from_slice(&[0x12; 16]);
    body.extend_from_slice(b"\x04Alex\x02\x08textures\x03abc\x01\x03sig\x05other\x00\x00");
    check_wire(&packet, &body);
}

#[test]
fn configuration_settings_keepalives_and_negotiation_match_wire() {
    check_wire(
        &ConfigurationClientInformation {
            locale: McString::new("en_US").unwrap(),
            view_distance: -1,
            chat_flags: VarInt(0),
            chat_colors: true,
            skin_parts: 0x7f,
            main_hand: VarInt(1),
            enable_text_filtering: false,
            enable_server_listing: true,
            particle_status: ConfigurationClientInformationParticleStatus::Decreased,
        },
        b"\x00\x05en_US\xff\x00\x01\x7f\x01\x00\x01\x01",
    );
    assert_eq!(ConfigurationClientInformation::STATE, State::Configuration);
    assert_eq!(ConfigurationClientInformation::DIRECTION, Direction::Serverbound);
    let wire = [4, 0, 1, 2, 3, 4, 5, 6, 7];
    check_wire(&ConfigurationKeepAlive { keep_alive_id: 0x0001_0203_0405_0607 }, &wire);
    check_wire(&ConfigurationKeepAliveResponse { keep_alive_id: 0x0001_0203_0405_0607 }, &wire);
    assert_eq!(ConfigurationKeepAlive::DIRECTION, Direction::Clientbound);
    check_wire(&FinishConfiguration, &[3]);
    check_wire(&AcknowledgeConfiguration, &[3]);
    check_wire(
        &SelectKnownPacks {
            packs: BoundedArray::new(vec![SelectKnownPacksPacksEntry {
                namespace: McString::new("minecraft").unwrap(),
                id: McString::new("core").unwrap(),
                version: McString::new("26.1").unwrap(),
            }])
            .unwrap(),
        },
        b"\x0e\x01\x09minecraft\x04core\x0426.1",
    );
    check_wire(&KnownPacks { packs: BoundedArray::new(vec![]).unwrap() }, &[7, 0]);
    check_wire(
        &FeatureFlags { features: BoundedArray::new(vec![McString::new("minecraft:vanilla").unwrap()]).unwrap() },
        b"\x0c\x01\x11minecraft:vanilla",
    );
}

#[test]
fn malformed_login_and_configuration_fields_are_rejected() {
    assert_eq!(decode_packet::<EncryptionResponse>(&[1, 0xff, 0xff, 0xff, 0xff, 0x0f]), Err(Error::CollectionTooLong));
    // 1 MiB + 1, rejected before reading or allocating the buffer.
    assert_eq!(decode_packet::<EncryptionResponse>(&[1, 0x81, 0x80, 0x40]), Err(Error::CollectionTooLong));
    assert_eq!(decode_packet::<LoginPluginResponse>(&[2, 0, 2]), Err(Error::InvalidBoolean));
    let invalid_enum = b"\x00\x05en_US\x08\x00\x01\x7f\x01\x00\x01\x03";
    assert_eq!(decode_packet::<ConfigurationClientInformation>(invalid_enum), Err(Error::InvalidEnumValue));
    assert_eq!(decode_packet::<KnownPacks>(&[7, 0x81, 8]), Err(Error::CollectionTooLong));
}

#[test]
fn optional_plugin_payload_distinguishes_absent_from_empty() {
    use chunk_protocol::RemainingBytes;

    for (data, wire) in [
        (None, vec![2, 42, 0]),
        (Some(RemainingBytes::new(vec![]).unwrap()), vec![2, 42, 1]),
        (Some(RemainingBytes::new(vec![0xab, 0xcd]).unwrap()), vec![2, 42, 1, 0xab, 0xcd]),
    ] {
        let packet = LoginPluginResponse { message_id: VarInt(42), data };
        assert_eq!(decode_packet::<LoginPluginResponse>(&wire).unwrap(), packet);
        let mut body = vec![2];
        packet.encode(&mut body).unwrap();
        assert_eq!(body, wire);
    }
}

#[test]
fn generated_limbo_packets_match_wire() {
    fn requires_eq<T: Eq>() {}

    check_wire(&GameEvent { reason: GameEventReason::LevelChunksLoadStart, value: 0.0 }, &[0x26, 13, 0, 0, 0, 0]);
    assert!(decode_packet::<GameEvent>(&[0x26, 255, 0, 0, 0, 0]).is_err());
    check_wire(&TitleTimes { fade_in: 10, stay: 200, fade_out: 20 }, &[0x73, 0, 0, 0, 10, 0, 0, 0, 200, 0, 0, 0, 20]);
    check_wire(
        &PlayClientInformation {
            locale: McString::new("en_US").unwrap(),
            view_distance: 8,
            chat_flags: VarInt(0),
            chat_colors: true,
            skin_parts: 127,
            main_hand: VarInt(1),
            enable_text_filtering: false,
            enable_server_listing: true,
            particle_status: PlayClientInformationParticleStatus::Minimal,
        },
        b"\x0e\x05en_US\x08\x00\x01\x7f\x01\x00\x01\x02",
    );
    let mut trailing = b"\x0e\x05en_US\x08\x00\x01\x7f\x01\x00\x01\x02".to_vec();
    trailing.push(0);
    assert!(decode_packet::<PlayClientInformation>(&trailing).is_err());
    requires_eq::<PlayKeepAlive>();
    requires_eq::<TitleTimes>();
}
