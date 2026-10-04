use chunk_protocol::{Decode, VarInt};
use tokio::{io::DuplexStream, task::JoinHandle};

use super::*;

fn information() -> ConfigurationClientInformation {
    use chunk_protocol::versions::v26_2::ConfigurationClientInformationParticleStatus;
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

fn pack(id: u8, sha1: char, required: bool) -> ResourcePack {
    ResourcePack {
        id: vec![id; 16],
        url: format!("https://packs.example/{id}"),
        sha1: sha1.to_string().repeat(40),
        required,
        prompt: String::new(),
    }
}

/// Applies `wanted` over `held` against an in-memory client, returning the client and what the connection holds after.
fn apply(held: Packs, wanted: Vec<ResourcePack>) -> (Transport<DuplexStream>, JoinHandle<(io::Result<()>, Packs)>) {
    let (client, server) = tokio::io::duplex(8192);
    let task = tokio::spawn(async move {
        let mut packs = held;
        let mut transport = Transport::new(server);
        let result = packs.apply(&mut transport, &mut information(), &wanted, Duration::from_secs(30)).await;
        (result, packs)
    });
    (Transport::new(client), task)
}

/// The UUID and SHA-1 of an `add_resource_pack` the client received.
async fn added(client: &mut Transport<DuplexStream>) -> ([u8; 16], String) {
    let frame = client.read_frame(4096).await.unwrap();
    let mut body = &frame[..];
    assert_eq!(VarInt::decode(&mut body).unwrap().0, AddResourcePack::ID);
    let uuid = Uuid::decode(&mut body).unwrap();
    McString::<32767>::decode(&mut body).unwrap();
    (uuid.0, McString::<40>::decode(&mut body).unwrap().as_str().to_owned())
}

async fn respond(client: &mut Transport<DuplexStream>, id: u8, result: i32) {
    client.write_packet(&ResourcePackResponse { uuid: Uuid([id; 16]), result: VarInt(result) }).await.unwrap();
}

async fn answer_keep_alive(client: &mut Transport<DuplexStream>) {
    let frame = client.read_frame(4096).await.unwrap();
    let id = decode_packet::<ConfigurationKeepAlive>(&frame).unwrap().keep_alive_id;
    client.write_packet(&ConfigurationKeepAliveResponse { keep_alive_id: id }).await.unwrap();
}

fn held(packs: &[(u8, char)]) -> Packs {
    Packs(packs.iter().map(|&(id, sha1)| ([id; 16], sha1.to_string().repeat(40))).collect())
}

async fn removed(client: &mut Transport<DuplexStream>) -> [u8; 16] {
    let frame = client.read_frame(4096).await.unwrap();
    decode_packet::<RemoveResourcePack>(&frame).unwrap().uuid.unwrap().0
}

#[tokio::test(start_paused = true)]
async fn moving_sessions_keeps_the_matching_prefix_and_resends_the_rest_in_order() {
    let wanted = vec![pack(1, 'a', true), pack(2, 'e', false), pack(4, 'd', false)];
    let (mut client, task) = apply(held(&[(1, 'a'), (2, 'b'), (3, 'c')]), wanted);

    assert_eq!(removed(&mut client).await, [2; 16]);
    assert_eq!(removed(&mut client).await, [3; 16]);
    assert_eq!(added(&mut client).await, ([2; 16], "e".repeat(40)));
    assert_eq!(added(&mut client).await, ([4; 16], "d".repeat(40)));
    answer_keep_alive(&mut client).await;
    respond(&mut client, 2, ACCEPTED).await;
    respond(&mut client, 2, DOWNLOADED).await;
    respond(&mut client, 2, LOADED).await;
    // An optional pack the player declines is skipped, and offered again on the next move.
    respond(&mut client, 4, 1).await;

    let (result, packs) = task.await.unwrap();
    result.unwrap();
    assert_eq!(packs.0, held(&[(1, 'a'), (2, 'e')]).0);
}

#[tokio::test(start_paused = true)]
async fn reordering_unchanged_packs_restacks_them() {
    let (mut client, task) = apply(held(&[(1, 'a'), (2, 'b')]), vec![pack(2, 'b', false), pack(1, 'a', false)]);

    assert_eq!(removed(&mut client).await, [1; 16]);
    assert_eq!(removed(&mut client).await, [2; 16]);
    assert_eq!(added(&mut client).await.0, [2; 16]);
    assert_eq!(added(&mut client).await.0, [1; 16]);
    answer_keep_alive(&mut client).await;
    // The stack follows the order the packs were sent, not the order they loaded.
    respond(&mut client, 1, LOADED).await;
    respond(&mut client, 2, LOADED).await;

    let (result, packs) = task.await.unwrap();
    result.unwrap();
    assert_eq!(packs.0, held(&[(2, 'b'), (1, 'a')]).0);
}

#[tokio::test(start_paused = true)]
async fn an_optional_pack_past_the_wait_is_withdrawn() {
    let (mut client, task) = apply(Packs::default(), vec![pack(6, 'g', false)]);
    added(&mut client).await;
    loop {
        let frame = client.read_frame(4096).await.unwrap();
        if let Ok(keep_alive) = decode_packet::<ConfigurationKeepAlive>(&frame) {
            let response = ConfigurationKeepAliveResponse { keep_alive_id: keep_alive.keep_alive_id };
            client.write_packet(&response).await.unwrap();
            continue;
        }
        assert_eq!(decode_packet::<RemoveResourcePack>(&frame).unwrap().uuid, Some(Uuid([6; 16])));
        break;
    }

    let (result, packs) = task.await.unwrap();
    result.unwrap();
    assert!(packs.0.is_empty());
}

#[tokio::test(start_paused = true)]
async fn declining_a_required_pack_disconnects() {
    let (mut client, task) = apply(Packs::default(), vec![pack(5, 'f', true)]);
    added(&mut client).await;
    respond(&mut client, 5, 1).await;

    let frame = client.read_frame(4096).await.unwrap();
    // The keepalive the wait sent precedes the disconnect.
    decode_packet::<ConfigurationKeepAlive>(&frame).unwrap();
    let frame = client.read_frame(4096).await.unwrap();
    assert_eq!(frame[0], 0x02);
    assert!(frame.ends_with(b"This server requires its resource pack."));
    let error = task.await.unwrap().0.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
}
