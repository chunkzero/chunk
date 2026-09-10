use super::*;
use chunk_protocol::{
    BoundedArray, Uuid,
    versions::v26_1::{ConfigurationClientInformationParticleStatus, FinishConfiguration, LoginSuccessPropertiesEntry},
};

#[tokio::test]
async fn native_login_presents_capability_and_preserves_profile_and_settings() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let prepared = PlayerPreparation {
        operation_id: "operation".into(),
        endpoint: listener.local_addr().unwrap().to_string(),
        capability: vec![7; 32],
    };
    let authenticated = Authenticated {
        protocol_version: 775,
        transport: Transport::new(tokio::io::empty()),
        profile: LoginSuccess {
            uuid: Uuid([1; 16]),
            username: McString::new("Alex").unwrap(),
            properties: BoundedArray::new(vec![LoginSuccessPropertiesEntry {
                name: McString::new("textures").unwrap(),
                value: McString::new("value").unwrap(),
                signature: Some(McString::new("signature").unwrap()),
            }])
            .unwrap(),
        },
    };
    let settings = ConfigurationClientInformation {
        locale: McString::new("en_US").unwrap(),
        view_distance: 8,
        chat_flags: VarInt(0),
        chat_colors: true,
        skin_parts: 127,
        main_hand: VarInt(1),
        enable_text_filtering: false,
        enable_server_listing: true,
        particle_status: ConfigurationClientInformationParticleStatus::All,
    };
    let expected = settings.clone();
    let profile = authenticated.profile.clone();
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut backend = Transport::new(socket);
        let handshake = decode_packet::<Handshake>(&backend.read_frame(4096).await.unwrap()).unwrap();
        assert_eq!(handshake.protocol_version, VarInt(775));
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
