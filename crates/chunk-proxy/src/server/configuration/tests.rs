use std::future::{pending, ready};

use chunk_protocol::{
    BoundedArray, McString, RemainingBytes, Uuid,
    versions::v26_2::{ConfigurationClientInformationParticleStatus, LoginSuccess},
};
use tokio::{io::DuplexStream, sync::oneshot, task::JoinHandle, time::advance};

use super::super::transport::Transport;
use super::*;

type Handoff = (Authenticated<DuplexStream>, ConfigurationClientInformation, &'static str);

fn information() -> ConfigurationClientInformation {
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

fn connection(capacity: usize) -> (Transport<DuplexStream>, Authenticated<DuplexStream>) {
    let (client, server) = tokio::io::duplex(capacity);
    let secret = [0x12; 16];
    let mut client = Transport::new(client);
    let mut server = Transport::new(server);
    for transport in [&mut client, &mut server] {
        transport.enable_encryption(&secret).unwrap();
        transport.enable_compression(256);
    }
    (
        client,
        Authenticated {
            protocol_version: 776,
            transport: server,
            profile: LoginSuccess {
                session_id: Uuid([2; 16]),
                uuid: Uuid([1; 16]),
                username: McString::new("Alex").unwrap(),
                properties: BoundedArray::new(vec![]).unwrap(),
            },
        },
    )
}

fn start_wait(max_wait: Duration) -> (Transport<DuplexStream>, oneshot::Sender<()>, JoinHandle<io::Result<Handoff>>) {
    let (client, authenticated) = connection(8192);
    let (ready, destination) = oneshot::channel();
    let server = tokio::spawn(wait_for_destination(
        authenticated,
        async {
            destination.await.map_err(io::Error::other)?;
            Ok("session-1")
        },
        max_wait,
    ));
    (client, ready, server)
}

async fn challenge(client: &mut Transport<DuplexStream>) -> i64 {
    decode_packet::<ConfigurationKeepAlive>(&client.read_frame(4096).await.unwrap()).unwrap().keep_alive_id
}

async fn answer(client: &mut Transport<DuplexStream>, id: i64) {
    client.write_packet(&ConfigurationKeepAliveResponse { keep_alive_id: id }).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn responsive_client_waits_then_hands_off_identity_settings_and_buffered_packets() {
    let (mut client, ready, server) = start_wait(Duration::from_secs(300));
    client.write_packet(&information()).await.unwrap();
    client
        .write_packet(&ConfigurationPluginResponse {
            channel: McString::new("minecraft:brand").unwrap(),
            data: RemainingBytes::new(b"\x07vanilla".to_vec()).unwrap(),
        })
        .await
        .unwrap();
    let mut previous_id = None;
    for _ in 0..8 {
        let id = challenge(&mut client).await;
        assert_ne!(previous_id, Some(id));
        previous_id = Some(id);
        answer(&mut client, id).await;
    }
    let id = challenge(&mut client).await;
    let mut latest = information();
    latest.locale = McString::new("de_DE").unwrap();
    client.write_packet(&latest).await.unwrap();
    ready.send(()).unwrap();
    tokio::task::yield_now().await;
    assert!(!server.is_finished(), "handoff must wait for the outstanding keepalive");
    answer(&mut client, id).await;
    let (mut authenticated, settings, destination) = server.await.unwrap().unwrap();
    assert_eq!(destination, "session-1");
    assert_eq!(authenticated.profile.uuid, Uuid([1; 16]));
    assert_eq!(settings.locale.as_str(), "de_DE");
    // The same encrypted transport remains usable by the next configuration stage.
    client.write_packet(&latest).await.unwrap();
    let frame = authenticated.transport.read_frame(4096).await.unwrap();
    assert_eq!(decode_packet::<ConfigurationClientInformation>(&frame).unwrap(), latest);
}

#[tokio::test(start_paused = true)]
async fn readiness_waits_for_client_information() {
    let (mut client, authenticated) = connection(8192);
    let server = tokio::spawn(wait_for_destination(authenticated, ready(Ok("ready")), Duration::from_secs(300)));
    let id = challenge(&mut client).await;
    answer(&mut client, id).await;
    tokio::task::yield_now().await;
    assert!(!server.is_finished());
    client.write_packet(&information()).await.unwrap();
    let (_, _, destination) = server.await.unwrap().unwrap();
    assert_eq!(destination, "ready");
}

#[tokio::test(start_paused = true)]
async fn missing_information_and_unanswered_keepalives_time_out() {
    for send_information in [false, true] {
        let (mut client, _ready, server) = start_wait(Duration::from_secs(300));
        if send_information {
            client.write_packet(&information()).await.unwrap();
        }
        let _ = challenge(&mut client).await;
        let error = server.await.unwrap().err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains(if send_information { "keepalive" } else { "information" }));
        assert_eq!(client.read_frame(4096).await.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }
}

#[tokio::test(start_paused = true)]
async fn wrong_duplicate_and_unsolicited_configuration_responses_are_rejected() {
    for duplicate in [false, true] {
        let (mut client, _ready, server) = start_wait(Duration::from_secs(300));
        client.write_packet(&information()).await.unwrap();
        let id = challenge(&mut client).await;
        if duplicate {
            answer(&mut client, id).await;
        }
        answer(&mut client, if duplicate { id } else { id + 1 }).await;
        assert_eq!(server.await.unwrap().err().unwrap().kind(), io::ErrorKind::InvalidData);
    }
    let (mut client, _ready, server) = start_wait(Duration::from_secs(300));
    client.write_packet(&chunk_protocol::versions::v26_2::AcknowledgeConfiguration).await.unwrap();
    assert_eq!(server.await.unwrap().err().unwrap().kind(), io::ErrorKind::InvalidData);
}

#[tokio::test(start_paused = true)]
async fn responsive_clients_still_obey_the_total_wait_limit() {
    let (mut client, _ready, server) = start_wait(Duration::from_secs(25));
    client.write_packet(&information()).await.unwrap();
    for _ in 0..3 {
        let id = challenge(&mut client).await;
        answer(&mut client, id).await;
    }
    let error = server.await.unwrap().err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(error.to_string().contains("wait expired"));
}

#[tokio::test(start_paused = true)]
async fn client_disconnect_destination_failure_and_task_cancellation_release_connections() {
    let (mut client, _ready, server) = start_wait(Duration::from_secs(300));
    challenge(&mut client).await;
    client.shutdown().await.unwrap();
    assert_eq!(server.await.unwrap().err().unwrap().kind(), io::ErrorKind::UnexpectedEof);

    let (mut client, ready, server) = start_wait(Duration::from_secs(300));
    challenge(&mut client).await;
    drop(ready);
    assert!(server.await.unwrap().is_err());
    assert_eq!(client.read_frame(4096).await.unwrap()[0], 0x02);
    assert_eq!(client.read_frame(4096).await.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);

    let (mut client, ready, server) = start_wait(Duration::from_secs(300));
    challenge(&mut client).await;
    server.abort();
    assert!(server.await.err().unwrap().is_cancelled());
    assert!(ready.is_closed());
    assert_eq!(client.read_frame(4096).await.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
}

#[tokio::test(start_paused = true)]
async fn blocked_writes_are_bounded_and_never_reused() {
    let (mut client, authenticated) = connection(1);
    let server =
        tokio::spawn(wait_for_destination(authenticated, pending::<io::Result<()>>(), Duration::from_secs(300)));
    tokio::task::yield_now().await;
    advance(WRITE_TIMEOUT).await;
    let error = server.await.unwrap().err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(client.read_frame(4096).await.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
}

#[tokio::test]
async fn destination_configuration_relays_settings_and_retains_coalesced_play() {
    use chunk_protocol::versions::v26_2::{FinishConfiguration, PlayPing};
    let (mut client, authenticated) = connection(8192);
    let (internal, backend) = tokio::io::duplex(8192);
    let mut backend = Transport::new(backend);
    let task = tokio::spawn(async move {
        let mut external = authenticated.transport;
        let mut internal = Transport::new(internal);
        relay(&mut external, &mut internal, &mut information()).await.unwrap();
        decode_packet::<PlayPing>(&external.read_frame(FRAME_LIMIT).await.unwrap()).unwrap()
    });
    client.write_packet(&information()).await.unwrap();
    assert_eq!(
        decode_packet::<ConfigurationClientInformation>(&backend.read_frame(FRAME_LIMIT).await.unwrap()).unwrap(),
        information()
    );
    backend.write_packet(&FinishConfiguration).await.unwrap();
    decode_packet::<FinishConfiguration>(&client.read_frame(FRAME_LIMIT).await.unwrap()).unwrap();
    client.write_packet(&AcknowledgeConfiguration).await.unwrap();
    client.write_packet(&PlayPing { id: 42 }).await.unwrap();
    decode_packet::<AcknowledgeConfiguration>(&backend.read_frame(FRAME_LIMIT).await.unwrap()).unwrap();
    assert_eq!(task.await.unwrap().id, 42);
}

#[tokio::test]
async fn destination_configuration_rejects_early_acknowledgment() {
    let (mut client, mut authenticated) = connection(8192);
    let (internal, _backend) = tokio::io::duplex(8192);
    client.write_packet(&AcknowledgeConfiguration).await.unwrap();
    let result = relay(&mut authenticated.transport, &mut Transport::new(internal), &mut information()).await;
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
}

#[tokio::test]
async fn admission_denial_is_a_configuration_disconnect_with_unicode_reason() {
    let (mut client, authenticated) = connection(8192);
    let server = tokio::spawn(wait_for_destination(
        authenticated,
        ready(Err::<(), _>(io::Error::new(io::ErrorKind::PermissionDenied, "Closed 🛠"))),
        Duration::from_secs(30),
    ));
    let frame = client.read_frame(4096).await.unwrap();
    assert_eq!(&frame[..4], &[0x02, 8, 0, 13]);
    assert_eq!(&frame[4..11], b"Closed ");
    assert_eq!(&frame[11..], &[0xed, 0xa0, 0xbd, 0xed, 0xbb, 0xa0]);
    assert_eq!(server.await.unwrap().err().unwrap().kind(), io::ErrorKind::PermissionDenied);
}
