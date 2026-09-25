use chunk_protocol::{
    BoundedArray, Encode, McString,
    versions::v26_2::{
        AcknowledgeConfiguration, ConfigurationClientInformationParticleStatus, FinishConfiguration, KnownPacks,
        SelectKnownPacks,
    },
};
use tokio::sync::oneshot;

use super::*;

fn information() -> ConfigurationClientInformation {
    ConfigurationClientInformation {
        locale: McString::new("de_DE").unwrap(),
        view_distance: 12,
        chat_flags: VarInt(0),
        chat_colors: true,
        skin_parts: 127,
        main_hand: VarInt(1),
        enable_text_filtering: false,
        enable_server_listing: true,
        particle_status: ConfigurationClientInformationParticleStatus::Minimal,
    }
}

#[tokio::test]
async fn cutover_discards_late_source_packets_and_eof_and_retains_settings_across_full_configuration() {
    for final_settings in [false, true] {
        let (client, server) = tokio::io::duplex(8192);
        let (jvm, internal) = tokio::io::duplex(8192);
        let mut client = Transport::new(client);
        let mut public = Transport::new(server);
        for transport in [&mut client, &mut public] {
            transport.enable_encryption(&[42; 16]).unwrap();
            transport.enable_compression(16);
        }
        let mut jvm = Transport::new(jvm);
        let mut internal = Transport::new(internal);
        let (ready, prepared) = oneshot::channel();
        let (stopped, stop_source) = oneshot::channel();
        let (late_sent, late) = oneshot::channel();
        let packets = vec![0x0c, if final_settings { 1 } else { 2 }, 0x04];
        let expected = packets.clone();
        let server = tokio::spawn(async move {
            let mut settings = information();
            settings.view_distance = 2;
            until(&mut public, &mut internal, &mut settings, prepared, true, None).await.unwrap().unwrap();
            stopped.send(()).unwrap();
            late.await.unwrap();
            start_configuration(&mut public, &mut internal, &mut settings, None).await.unwrap();
            drop(internal);
            let (destination, backend) = tokio::io::duplex(8192);
            let mut destination = Transport::new(destination);
            let mut backend = Transport::new(backend);
            let backend = tokio::spawn(async move {
                backend.write_packet(&SelectKnownPacks { packs: BoundedArray::new(vec![]).unwrap() }).await.unwrap();
                decode_packet::<KnownPacks>(&backend.read_frame(4096).await.unwrap()).unwrap();
                backend.write_body(&packets).await.unwrap();
                backend.write_packet(&FinishConfiguration).await.unwrap();
                decode_packet::<AcknowledgeConfiguration>(&backend.read_frame(4096).await.unwrap()).unwrap();
            });
            configuration::relay(&mut public, &mut destination, &mut settings).await.unwrap();
            backend.await.unwrap();
            settings
        });
        let mut body = Vec::new();
        VarInt(PlayClientInformation::ID).encode(&mut body).unwrap();
        information().encode(&mut body).unwrap();
        client.write_body(&body).await.unwrap();
        assert_eq!(jvm.read_frame(4096).await.unwrap().as_ref(), body);
        jvm.write_body(&[0x7f, 42]).await.unwrap();
        assert_eq!(client.read_frame(4096).await.unwrap().as_ref(), &[0x7f, 42]);
        ready.send(()).unwrap();
        stop_source.await.unwrap();
        jvm.write_body(&[0x7e, 99]).await.unwrap();
        jvm.shutdown().await.unwrap();
        late_sent.send(()).unwrap();
        decode_packet::<StartConfiguration>(&client.read_frame(4096).await.unwrap()).unwrap();
        // This remains a source PLAY packet, not a configuration packet.
        client.write_body(&[0x05, 0x01]).await.unwrap();
        assert_eq!(jvm.read_frame(4096).await.unwrap().as_ref(), &[0x05, 0x01]);
        if final_settings {
            let mut settings = information();
            settings.view_distance = 7;
            let mut body = Vec::new();
            VarInt(PlayClientInformation::ID).encode(&mut body).unwrap();
            settings.encode(&mut body).unwrap();
            client.write_body(&body).await.unwrap();
            assert_eq!(jvm.read_frame(4096).await.unwrap().as_ref(), body);
        }
        client.write_packet(&ConfigurationAcknowledged).await.unwrap();
        assert_eq!(jvm.read_frame(4096).await.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        decode_packet::<SelectKnownPacks>(&client.read_frame(4096).await.unwrap()).unwrap();
        client.write_packet(&KnownPacks { packs: BoundedArray::new(vec![]).unwrap() }).await.unwrap();
        assert_eq!(client.read_frame(4096).await.unwrap().as_ref(), expected);
        decode_packet::<FinishConfiguration>(&client.read_frame(4096).await.unwrap()).unwrap();
        client.write_packet(&AcknowledgeConfiguration).await.unwrap();
        let retained = server.await.unwrap();
        assert_eq!(retained.locale.as_str(), "de_DE");
        assert_eq!(retained.view_distance, if final_settings { 7 } else { 12 });
    }
}

#[tokio::test]
async fn active_destination_failure_sends_a_play_disconnect() {
    let (client, public) = tokio::io::duplex(8192);
    let (jvm, internal) = tokio::io::duplex(8192);
    let mut client = Transport::new(client);
    let server = tokio::spawn(async move {
        until(
            &mut Transport::new(public),
            &mut Transport::new(internal),
            &mut information(),
            std::future::pending::<()>(),
            true,
            None,
        )
        .await
    });
    drop(jvm);
    let packet = client.read_frame(4096).await.unwrap();
    assert_eq!(packet[0], 0x20);
    assert_eq!(server.await.unwrap().unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
}

/// Relays gameplay output to a player reading `chunk` bytes per `period`, for at most a minute.
async fn relay_to_reader(period: std::time::Duration, chunk: usize) -> (Option<io::Error>, usize) {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::AsyncReadExt;

    let (mut client, public) = tokio::io::duplex(1024);
    let (jvm, internal) = tokio::io::duplex(8192);
    let relay = tokio::spawn(async move {
        until(
            &mut Transport::new(public),
            &mut Transport::new(internal),
            &mut information(),
            std::future::pending::<()>(),
            true,
            None,
        )
        .await
    });
    let sent = Arc::new(AtomicUsize::new(0));
    let writer = tokio::spawn({
        let sent = sent.clone();
        async move {
            let mut jvm = Transport::new(jvm);
            while sent.load(Ordering::Relaxed) < 8 * BACKLOG_LIMIT && jvm.write_body(&[0x7f; 16_384]).await.is_ok() {
                sent.fetch_add(16_384, Ordering::Relaxed);
            }
            std::future::pending::<()>().await;
        }
    });
    let reader = tokio::spawn(async move {
        let mut bytes = vec![0; chunk];
        loop {
            tokio::time::sleep(period).await;
            if matches!(client.read(&mut bytes).await, Ok(0) | Err(_)) {
                return;
            }
        }
    });
    let result = tokio::time::timeout(std::time::Duration::from_secs(60), relay).await;
    writer.abort();
    reader.abort();
    (result.ok().map(|result| result.unwrap().unwrap_err()), sent.load(Ordering::Relaxed))
}

#[tokio::test(start_paused = true)]
async fn slow_players_bound_gameplay_reads_and_only_trickling_ones_time_out() {
    use std::time::Duration;

    let (error, sent) = relay_to_reader(Duration::from_secs(1), 256).await;
    assert_eq!(error.unwrap().kind(), io::ErrorKind::TimedOut);
    assert!(sent < 2 * BACKLOG_LIMIT);
    // About 10 KiB/s: slow, but drains faster than the minimum write progress.
    let (error, sent) = relay_to_reader(Duration::from_millis(100), 1024).await;
    assert!(error.is_none());
    assert!(sent > 2 * BACKLOG_LIMIT);
}
