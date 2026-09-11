use chunk_protocol::{
    Decode, VarInt,
    versions::v26_1::{AcknowledgeConfiguration, KnownPacks},
};
use std::future::pending;

use chunk_protocol::{
    BoundedArray, McString, Uuid,
    versions::v26_1::{
        ChunkBatchFinished, ChunkBatchStart, ConfigurationClientInformationParticleStatus, ConfigurationKeepAlive,
        ConfigurationKeepAliveResponse, FeatureFlags, FinishConfiguration, GameEvent, LIMBO_REGISTRIES, LIMBO_TAGS,
        LoginSuccess, MovePosition, PlayerAbilities, SelectKnownPacks, SetChunkCenter, SynchronizePosition, TickEnd,
        TitleTimes,
    },
};
use tokio::{io::DuplexStream, sync::oneshot};

use super::*;

use super::world::{JoinLimbo, LimboChunk, SPAWN};

fn cache() -> &'static Cache {
    static CACHE: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| Cache::new(Some(256)).unwrap())
}

fn packets() -> &'static Packets {
    cache().get(775).unwrap()
}

async fn wait_for_destination<T>(
    authenticated: Authenticated<DuplexStream>,
    destination: impl Future<Output = io::Result<T>>,
    configuration_timeout: Duration,
) -> io::Result<(Authenticated<DuplexStream>, ConfigurationClientInformation, T)> {
    super::wait_for_destination(authenticated, destination, configuration_timeout, cache()).await
}

fn connection() -> (Transport<DuplexStream>, Authenticated<DuplexStream>) {
    let (client, server) = tokio::io::duplex(131_072);
    let mut client = Transport::new(client);
    let mut server = Transport::new(server);
    for transport in [&mut client, &mut server] {
        transport.enable_encryption(&[42; 16]).unwrap();
        transport.enable_compression(256);
    }
    (
        client,
        Authenticated {
            protocol_version: 775,
            transport: server,
            profile: LoginSuccess {
                uuid: Uuid([1; 16]),
                username: McString::new("Alex").unwrap(),
                properties: BoundedArray::new(vec![]).unwrap(),
            },
        },
    )
}

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

async fn enter(client: &mut Transport<DuplexStream>) {
    client.write_packet(&information()).await.unwrap();
    loop {
        let frame = client.read_frame(FRAME_LIMIT).await.unwrap();
        match packet_id(&frame).unwrap() {
            ConfigurationKeepAlive::ID => {
                let keep_alive = decode_packet::<ConfigurationKeepAlive>(&frame).unwrap();
                client
                    .write_packet(&ConfigurationKeepAliveResponse { keep_alive_id: keep_alive.keep_alive_id })
                    .await
                    .unwrap();
            }
            SelectKnownPacks::ID => break,
            id => panic!("unexpected configuration packet {id}"),
        }
    }
    client.write_packet(&KnownPacks { packs: BoundedArray::new(vec![]).unwrap() }).await.unwrap();
    let mut registries = 0;
    loop {
        let frame = client.read_frame(chunk_protocol::MAX_FRAME_SIZE).await.unwrap();
        match packet_id(&frame).unwrap() {
            7 => registries += 1,
            FinishConfiguration::ID => break,
            FeatureFlags::ID | 0x0d => {}
            id => panic!("unexpected registry exchange packet {id}"),
        }
    }
    assert_eq!(registries, LIMBO_REGISTRIES.len());
    client.write_packet(&AcknowledgeConfiguration).await.unwrap();
    let mut chunks = 0;
    loop {
        let frame = client.read_frame(chunk_protocol::MAX_FRAME_SIZE).await.unwrap();
        match packet_id(&frame).unwrap() {
            LimboChunk::ID => chunks += 1,
            ChunkBatchFinished::ID => {
                assert_eq!(decode_packet::<ChunkBatchFinished>(&frame).unwrap().batch_size.0, chunks);
                client.write_packet(&ChunkBatchReceived { chunks_per_tick: 20.0 }).await.unwrap();
            }
            SynchronizePosition::ID => {
                let spawn = decode_packet::<SynchronizePosition>(&frame).unwrap();
                assert_eq!([spawn.x, spawn.y, spawn.z].map(f64::to_bits), SPAWN.map(f64::to_bits));
                client.write_packet(&ConfirmTeleport { teleport_id: spawn.teleport_id }).await.unwrap();
                client.write_packet(&PlayerLoaded).await.unwrap();
                break;
            }
            JoinLimbo::ID | PlayerAbilities::ID | SetChunkCenter::ID | GameEvent::ID | ChunkBatchStart::ID => {}
            id => panic!("unexpected spawn packet {id}"),
        }
    }
    assert_eq!(chunks, 25);
    // Keepalive can precede the client's loaded notification.
    loop {
        let frame = client.read_frame(FRAME_LIMIT).await.unwrap();
        match packet_id(&frame).unwrap() {
            PlayKeepAlive::ID => {
                let id = decode_packet::<PlayKeepAlive>(&frame).unwrap().keep_alive_id;
                answer(client, id).await;
            }
            TitleTimes::ID => {
                let timing = decode_packet::<TitleTimes>(&frame).unwrap();
                assert_eq!((timing.fade_in, timing.stay, timing.fade_out), (10, 200, 20));
                break;
            }
            id => panic!("unexpected packet before title {id}"),
        }
    }
    assert_eq!(client.read_frame(FRAME_LIMIT).await.unwrap().as_ref(), b"\x72\x08\x00\x18Preparing your server...");
}

async fn heartbeat(client: &mut Transport<DuplexStream>) -> i64 {
    decode_packet::<PlayKeepAlive>(&client.read_frame(FRAME_LIMIT).await.unwrap()).unwrap().keep_alive_id
}

async fn answer(client: &mut Transport<DuplexStream>, id: i64) {
    client.write_packet(&PlayKeepAliveResponse { keep_alive_id: id }).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn encrypted_client_ignores_movement_and_hands_off_after_keepalive() {
    let (mut client, authenticated) = connection();
    let (ready, destination) = oneshot::channel();
    let server = tokio::spawn(wait_for_destination(
        authenticated,
        async { destination.await.map_err(io::Error::other) },
        Duration::from_secs(30),
    ));
    enter(&mut client).await;
    for _ in 0..3 {
        let id = heartbeat(&mut client).await;
        answer(&mut client, id).await;
        client.write_packet(&MovePosition { x: SPAWN[0], y: 64.0, z: SPAWN[2], flags: 1 }).await.unwrap();
    }
    let id = heartbeat(&mut client).await;
    client.write_packet(&MovePosition { x: 100.0, y: 64.0, z: SPAWN[2], flags: 1 }).await.unwrap();
    client
        .write_packet(&PlayClientInformation {
            locale: McString::new("fr_FR").unwrap(),
            view_distance: 12,
            chat_flags: VarInt(0),
            chat_colors: true,
            skin_parts: 127,
            main_hand: VarInt(1),
            enable_text_filtering: false,
            enable_server_listing: true,
            particle_status: PlayClientInformationParticleStatus::Minimal,
        })
        .await
        .unwrap();
    ready.send("session-1").unwrap();
    tokio::task::yield_now().await;
    assert!(!server.is_finished(), "handoff must drain the keepalive response");
    answer(&mut client, id).await;
    let (mut authenticated, settings, destination) = server.await.unwrap().unwrap();
    assert_eq!(destination, "session-1");
    assert_eq!(settings.locale.as_str(), "fr_FR");
    assert_eq!(settings.view_distance, 12);
    assert_eq!(settings.particle_status, ConfigurationClientInformationParticleStatus::Minimal);
    assert_eq!(authenticated.profile.uuid, Uuid([1; 16]));
    client.write_packet(&TickEnd).await.unwrap();
    decode_packet::<TickEnd>(&authenticated.transport.read_frame(FRAME_LIMIT).await.unwrap()).unwrap();
}

#[tokio::test(start_paused = true)]
async fn play_keepalive_rejects_wrong_ids_and_times_out_silent_clients() {
    for wrong_id in [false, true] {
        let (mut client, authenticated) = connection();
        let server =
            tokio::spawn(wait_for_destination(authenticated, pending::<io::Result<()>>(), Duration::from_secs(30)));
        enter(&mut client).await;
        let id = heartbeat(&mut client).await;
        if wrong_id {
            answer(&mut client, id + 1).await;
        }
        let error = server.await.unwrap().err().unwrap();
        assert_eq!(error.kind(), if wrong_id { io::ErrorKind::InvalidData } else { io::ErrorKind::TimedOut });
        assert!(error.to_string().contains("keepalive"));
        assert_eq!(client.read_frame(FRAME_LIMIT).await.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }
}

#[tokio::test(start_paused = true)]
async fn configuration_completion_and_spawn_acknowledgments_are_required() {
    let (mut client, authenticated) = connection();
    let mut settings = information();
    let server = tokio::spawn(async move {
        let mut transport = authenticated.transport;
        timeout(
            ACK_TIMEOUT,
            configuration::finish(&mut transport, &mut settings, &packets().known_packs, &packets().configuration),
        )
        .await
    });
    client.read_frame(FRAME_LIMIT).await.unwrap();
    client.write_packet(&KnownPacks { packs: BoundedArray::new(vec![]).unwrap() }).await.unwrap();
    loop {
        if packet_id(&client.read_frame(chunk_protocol::MAX_FRAME_SIZE).await.unwrap()).unwrap()
            == FinishConfiguration::ID
        {
            break;
        }
    }
    assert!(server.await.unwrap().is_err());

    let (mut client, authenticated) = connection();
    let server = tokio::spawn(play(authenticated, information(), pending::<io::Result<()>>(), packets()));
    let id = heartbeat(&mut client).await;
    answer(&mut client, id).await;
    let id = heartbeat(&mut client).await;
    answer(&mut client, id).await;
    assert!(server.await.unwrap().err().unwrap().to_string().contains("teleport"));
}

#[tokio::test(start_paused = true)]
async fn disconnect_and_cancellation_release_play_connections() {
    for cancel in [false, true] {
        let (mut client, authenticated) = connection();
        let server =
            tokio::spawn(wait_for_destination(authenticated, pending::<io::Result<()>>(), Duration::from_secs(30)));
        enter(&mut client).await;
        heartbeat(&mut client).await;
        if cancel {
            server.abort();
            assert!(server.await.err().unwrap().is_cancelled());
        } else {
            client.shutdown().await.unwrap();
            assert_eq!(server.await.unwrap().err().unwrap().kind(), io::ErrorKind::UnexpectedEof);
        }
        assert_eq!(client.read_frame(FRAME_LIMIT).await.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }
}

#[test]
fn limbo_registries_bind_dimension_and_client_component_dependencies() {
    let mut empty_registries = Vec::new();
    for mut frame in LIMBO_REGISTRIES.iter().copied() {
        VarInt::decode(&mut frame).unwrap();
        assert_eq!(VarInt::decode(&mut frame).unwrap().0, 7);
        let name = McString::<32767>::decode(&mut frame).unwrap();
        if ["minecraft:enchantment", "minecraft:dialog"].contains(&name.as_str()) {
            assert_eq!(VarInt::decode(&mut frame).unwrap().0, 0);
            assert!(frame.is_empty());
            empty_registries.push(name);
        }
    }
    assert_eq!(empty_registries.len(), 2);
    let mut frame = LIMBO_TAGS;
    VarInt::decode(&mut frame).unwrap();
    assert_eq!(VarInt::decode(&mut frame).unwrap().0, 0x0d);
    let registry_count = VarInt::decode(&mut frame).unwrap().0;
    let snapshot: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../chunk-protocol/data/26.1/loginPacket.json"
    )))
    .unwrap();
    let mut tags = std::collections::BTreeMap::new();
    for _ in 0..registry_count {
        let registry = McString::<32767>::decode(&mut frame).unwrap();
        let entries = snapshot["dimensionCodec"][registry.as_str()]["entries"].as_array().unwrap();
        let count = VarInt::decode(&mut frame).unwrap().0;
        for _ in 0..count {
            let name = McString::<32767>::decode(&mut frame).unwrap();
            let members = BoundedArray::<VarInt, 64>::decode(&mut frame).unwrap();
            let members = members
                .as_slice()
                .iter()
                .map(|id| entries[usize::try_from(id.0).unwrap()]["key"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>();
            tags.insert(format!("{}/{}", registry.as_str(), name.as_str()), members);
        }
    }
    assert_eq!(
        tags["minecraft:timeline/minecraft:in_overworld"],
        ["minecraft:villager_schedule", "minecraft:day", "minecraft:moon", "minecraft:early_game",]
    );
    assert_eq!(tags["minecraft:timeline/minecraft:in_nether"], ["minecraft:villager_schedule"]);
    assert_eq!(tags["minecraft:timeline/minecraft:in_end"], ["minecraft:villager_schedule"]);
    assert_eq!(
        tags["minecraft:damage_type/minecraft:is_fire"],
        [
            "minecraft:in_fire",
            "minecraft:campfire",
            "minecraft:on_fire",
            "minecraft:lava",
            "minecraft:hot_floor",
            "minecraft:unattributed_fireball",
            "minecraft:fireball",
        ]
    );
    for name in ["is_explosion", "bypasses_shield"] {
        assert!(!tags[&format!("minecraft:damage_type/minecraft:{name}")].is_empty());
    }
    for (item, pattern) in [
        ("bordure_indented", "curly_border"),
        ("field_masoned", "bricks"),
        ("creeper", "creeper"),
        ("flow", "flow"),
        ("flower", "flower"),
        ("globe", "globe"),
        ("guster", "guster"),
        ("mojang", "mojang"),
        ("piglin", "piglin"),
        ("skull", "skull"),
    ] {
        assert_eq!(
            tags[&format!("minecraft:banner_pattern/minecraft:pattern_item/{item}")],
            [format!("minecraft:{pattern}")]
        );
    }
    assert!(frame.is_empty());
}

#[tokio::test(start_paused = true)]
async fn responsive_connections_are_evicted_after_one_minute() {
    let (mut client, authenticated) = connection();
    let started = Instant::now();
    let server =
        tokio::spawn(wait_for_destination(authenticated, pending::<io::Result<()>>(), Duration::from_secs(300)));
    enter(&mut client).await;
    loop {
        match client.read_frame(FRAME_LIMIT).await {
            Ok(frame) => {
                let id = decode_packet::<PlayKeepAlive>(&frame).unwrap().keep_alive_id;
                if let Err(error) = client.write_packet(&PlayKeepAliveResponse { keep_alive_id: id }).await {
                    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
                    break;
                }
            }
            Err(error) => {
                assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
                break;
            }
        }
    }
    let error = server.await.unwrap().err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(error.to_string(), "limbo waiting limit reached");
    assert_eq!(Instant::now() - started, Duration::from_secs(60));
}

#[tokio::test(start_paused = true)]
async fn configuration_cannot_extend_the_one_minute_limit() {
    let (mut client, authenticated) = connection();
    let started = Instant::now();
    let server =
        tokio::spawn(wait_for_destination(authenticated, pending::<io::Result<()>>(), Duration::from_secs(300)));
    client.write_packet(&information()).await.unwrap();
    loop {
        let frame = client.read_frame(FRAME_LIMIT).await.unwrap();
        if packet_id(&frame).unwrap() == SelectKnownPacks::ID {
            break;
        }
        let id = decode_packet::<ConfigurationKeepAlive>(&frame).unwrap().keep_alive_id;
        client.write_packet(&ConfigurationKeepAliveResponse { keep_alive_id: id }).await.unwrap();
    }
    let error = server.await.unwrap().err().unwrap();
    assert_eq!(error.to_string(), "limbo waiting limit reached");
    assert_eq!(Instant::now() - started, Duration::from_secs(60));
}
