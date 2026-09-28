use super::*;
use crate::server::transport::{WRITE_TIMEOUT, within};
use chunk_protocol::{
    BoundedArray, Uuid,
    versions::v26_2::{ConfigurationClientInformationParticleStatus, FinishConfiguration, LoginSuccessPropertiesEntry},
};

#[tokio::test]
async fn native_login_presents_capability_and_preserves_profile_and_settings() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let prepared = PlayerPreparation {
        operation_id: "operation".into(),
        endpoint: listener.local_addr().unwrap().to_string(),
        capability: vec![7; 32],
    };
    let authenticated = authenticated();
    let settings = settings();
    let expected = settings.clone();
    let mut profile = authenticated.profile.clone();
    profile.session_id = Uuid([3; 16]);
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut backend = Transport::new(socket);
        let handshake = decode_packet::<Handshake>(&backend.read_frame(4096).await.unwrap()).unwrap();
        assert_eq!(handshake.protocol_version, VarInt(776));
        assert_eq!(handshake.next_state, VarInt(2));
        let start = decode_packet::<LoginStart>(&backend.read_frame(4096).await.unwrap()).unwrap();
        assert_eq!(start.player_uuid, profile.uuid);
        assert_eq!(start.username, profile.username);
        backend
            .write_packet(&LoginPluginRequest {
                message_id: VarInt(17),
                channel: McString::new("chunk:delivery").unwrap(),
                data: RemainingBytes::new(vec![]).unwrap(),
            })
            .await
            .unwrap();
        let response = decode_packet::<LoginPluginResponse>(&backend.read_frame(4096).await.unwrap()).unwrap();
        assert_eq!(response.message_id, VarInt(17));
        let setup = PlayerSetup::decode(response.data.unwrap().as_slice()).unwrap();
        assert_eq!(setup.operation_id, "operation");
        assert_eq!(setup.capability, vec![7; 32]);
        backend.write_packet(&profile).await.unwrap();
        backend.write_packet(&FinishConfiguration).await.unwrap();
        decode_packet::<LoginAcknowledged>(&backend.read_frame(4096).await.unwrap()).unwrap();
        assert_eq!(
            decode_packet::<ConfigurationClientInformation>(&backend.read_frame(4096).await.unwrap()).unwrap(),
            expected
        );
    });
    let mut internal = within(WRITE_TIMEOUT, login(&authenticated, &settings, prepared)).await.unwrap();
    decode_packet::<FinishConfiguration>(&internal.read_frame(4096).await.unwrap()).unwrap();
    task.await.unwrap();
}

fn authenticated() -> Authenticated<tokio::io::Empty> {
    Authenticated {
        protocol_version: 776,
        transport: Transport::new(tokio::io::empty()),
        profile: LoginSuccess {
            session_id: Uuid([2; 16]),
            uuid: Uuid([1; 16]),
            username: McString::new("Alex").unwrap(),
            properties: BoundedArray::new(vec![LoginSuccessPropertiesEntry {
                name: McString::new("textures").unwrap(),
                value: McString::new("value").unwrap(),
                signature: Some(McString::new("signature").unwrap()),
            }])
            .unwrap(),
        },
    }
}

fn settings() -> ConfigurationClientInformation {
    ConfigurationClientInformation {
        locale: McString::new("en_US").unwrap(),
        view_distance: 8,
        chat_flags: VarInt(0),
        chat_colors: true,
        skin_parts: 127,
        main_hand: VarInt(1),
        enable_text_filtering: false,
        enable_server_listing: true,
        particle_status: ConfigurationClientInformationParticleStatus::All,
    }
}

#[tokio::test]
async fn dials_only_private_endpoints() {
    for endpoint in ["127.0.0.1:25565", "10.1.2.3:25565", "[fdaa::1]:25565", "[::ffff:192.168.0.2]:25565"] {
        assert!(destination(endpoint).is_ok(), "{endpoint}");
    }
    for endpoint in
        ["203.0.113.1:25565", "169.254.169.254:80", "[fe80::1]:25565", "[2001:db8::1]:25565", "0.0.0.0:25565"]
    {
        assert_eq!(destination(endpoint).unwrap_err().kind(), io::ErrorKind::InvalidData, "{endpoint}");
    }
    let prepared = PlayerPreparation {
        operation_id: "operation".into(),
        endpoint: "203.0.113.1:25565".into(),
        capability: vec![7; 32],
    };
    let Err(error) = within(WRITE_TIMEOUT, login(&authenticated(), &settings(), prepared)).await else {
        panic!("a public gameplay endpoint was dialed");
    };
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}
