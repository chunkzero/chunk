use super::*;
use chunk_protocol::{
    Decode, Encode, McString, Packet, VarInt,
    commands::{CommandNode, NodeKind, SignedCommand, SystemMessage},
    decode_packet,
    versions::v26_1::{ConfigurationClientInformation, ConfigurationClientInformationParticleStatus, UnsignedCommand},
};
use fixture::Fixture;
use std::{sync::atomic::Ordering, time::Duration};
use tokio::io::DuplexStream;

fn unsigned(command: &str) -> Vec<u8> {
    let frame = encode_packet(&UnsignedCommand { command: McString::new(command).unwrap() }).unwrap();
    body(&frame).to_vec()
}
fn body(mut frame: &[u8]) -> &[u8] {
    VarInt::decode(&mut frame).unwrap();
    frame
}
fn signed(command: &str) -> Vec<u8> {
    let mut frame = Vec::new();
    VarInt(SignedCommand::ID).encode(&mut frame).unwrap();
    McString::<1024>::new(command).unwrap().encode(&mut frame).unwrap();
    123_i64.encode(&mut frame).unwrap();
    456_i64.encode(&mut frame).unwrap();
    VarInt(0).encode(&mut frame).unwrap();
    VarInt(7).encode(&mut frame).unwrap();
    frame.extend_from_slice(&[1, 2, 3, 4]);
    frame
}
async fn output(commands: &mut Commands, public: &mut Transport<DuplexStream>) {
    let event = tokio::time::timeout(Duration::from_secs(3), commands.receive()).await.unwrap();
    commands.publish(event, public).await.unwrap();
}
async fn wait_count(count: &std::sync::atomic::AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while count.load(Ordering::SeqCst) < expected {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
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
        particle_status: ConfigurationClientInformationParticleStatus::Minimal,
    }
}

#[tokio::test]
async fn relay_keeps_pumping_while_owned_commands_await_and_preserves_jvm_signed_bytes() {
    let mut fixture = Fixture::new().await;
    let (client, public) = tokio::io::duplex(16384);
    let (jvm, internal) = tokio::io::duplex(16384);
    let mut client = Transport::new(client);
    let mut public = Transport::new(public);
    let mut jvm = Transport::new(jvm);
    let mut internal = Transport::new(internal);
    fixture.commands.tree(CommandTree::empty()).unwrap();
    fixture.commands.arrived();
    output(&mut fixture.commands, &mut public).await;
    let published = decode_packet::<CommandTree>(&client.read_frame(16384).await.unwrap()).unwrap();
    assert_eq!(published.nodes[published.root].children.len(), 4);
    let (done, ready) = oneshot::channel();
    let service = fixture.service.clone();
    let traffic = async {
        jvm.write_packet(&CommandTree::empty()).await.unwrap();
        let tree = decode_packet::<CommandTree>(&client.read_frame(16384).await.unwrap()).unwrap();
        assert_eq!(tree.nodes[0].children.len(), 4);
        client.write_body(&unsigned("slow")).await.unwrap();
        wait_count(&service.waiting, 1).await;
        jvm.write_body(&[0x7f, 42]).await.unwrap();
        assert_eq!(client.read_frame(16384).await.unwrap().as_ref(), &[0x7f, 42]);
        let signed = signed("jvm hello");
        client.write_body(&signed).await.unwrap();
        assert_eq!(jvm.read_frame(16384).await.unwrap().as_ref(), signed);
        // The proxy owns this root even though it is carried in a signed envelope.
        client.write_body(&signed_owned()).await.unwrap();
        assert_eq!(VarInt::decode(&mut client.read_frame(16384).await.unwrap().as_ref()).unwrap().0, SystemMessage::ID);
        service.release.notify_one();
        assert_eq!(VarInt::decode(&mut client.read_frame(16384).await.unwrap().as_ref()).unwrap().0, SystemMessage::ID);
        wait_count(&service.replies, 1).await;
        assert_eq!(service.prepared.load(Ordering::SeqCst), 1);
        done.send(()).unwrap();
    };
    let mut settings = settings();
    let relay =
        super::super::relay::until(&mut public, &mut internal, &mut settings, ready, true, Some(&mut fixture.commands));
    let ((), result) = tokio::join!(traffic, relay);
    result.unwrap().unwrap();
    fixture.close().await;
}
fn signed_owned() -> Vec<u8> {
    signed("echo")
}

#[tokio::test]
async fn ownership_is_hidden_before_arrival_and_permissions_are_fresh_at_dispatch() {
    let mut fixture = Fixture::new().await;
    let (client, public) = tokio::io::duplex(16384);
    let mut public = Transport::new(public);
    let mut client = Transport::new(client);
    let hidden = fixture.commands.tree(CommandTree::empty()).unwrap();
    assert!(decode_packet::<CommandTree>(body(&hidden)).unwrap().nodes[0].children.is_empty());
    assert!(fixture.commands.input(&unsigned("echo")).unwrap());
    output(&mut fixture.commands, &mut public).await; // Pre-arrival text rejected, never sent in configuration.
    assert_eq!(fixture.service.prepared.load(Ordering::SeqCst), 0);
    fixture.commands.arrived();
    output(&mut fixture.commands, &mut public).await;
    client.read_frame(16384).await.unwrap();
    fixture.service.allowed.store(false, Ordering::SeqCst);
    assert!(fixture.commands.input(&unsigned("echo")).unwrap());
    output(&mut fixture.commands, &mut public).await;
    client.read_frame(16384).await.unwrap();
    assert_eq!(fixture.service.prepared.load(Ordering::SeqCst), 0);
    fixture.commands.refresh();
    output(&mut fixture.commands, &mut public).await;
    let hidden = decode_packet::<CommandTree>(&client.read_frame(16384).await.unwrap()).unwrap();
    assert!(hidden.nodes[0].children.is_empty());
    assert!(fixture.commands.input(&unsigned("echo bad args")).unwrap());
    let collision = CommandTree {
        root: 0,
        nodes: vec![
            CommandNode { children: vec![1], ..CommandNode::new(NodeKind::Root) },
            CommandNode::new(NodeKind::Literal { name: McString::new("echo").unwrap() }),
        ],
    };
    assert!(fixture.commands.tree(collision).is_err());
    fixture.close().await;
}

#[tokio::test]
async fn failed_permission_refresh_keeps_last_catalog() {
    let mut fixture = Fixture::new().await;
    let (client, public) = tokio::io::duplex(16384);
    let mut public = Transport::new(public);
    let mut client = Transport::new(client);
    fixture.commands.tree(CommandTree::empty()).unwrap();
    fixture.commands.arrived();
    output(&mut fixture.commands, &mut public).await;
    let published = decode_packet::<CommandTree>(&client.read_frame(16384).await.unwrap()).unwrap();
    assert_eq!(published.nodes[0].children.len(), 4);
    fixture.service.catalog_unavailable.store(true, Ordering::SeqCst);
    fixture.commands.refresh();
    output(&mut fixture.commands, &mut public).await;
    assert!(!fixture.commands.allowed.is_empty());
    assert!(tokio::time::timeout(Duration::from_millis(100), client.read_frame(16384)).await.is_err());
    fixture.service.catalog_unavailable.store(false, Ordering::SeqCst);
    fixture.service.allowed.store(false, Ordering::SeqCst);
    fixture.commands.refresh();
    output(&mut fixture.commands, &mut public).await;
    let hidden = decode_packet::<CommandTree>(&client.read_frame(16384).await.unwrap()).unwrap();
    assert!(hidden.nodes[0].children.is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn cutover_cancels_default_command_and_follow_text_uses_same_connections_new_claim() {
    let mut fixture = Fixture::new().await;
    let (client, public) = tokio::io::duplex(16384);
    let mut public = Transport::new(public);
    let mut client = Transport::new(client);
    fixture.commands.tree(CommandTree::empty()).unwrap();
    fixture.commands.arrived();
    output(&mut fixture.commands, &mut public).await;
    client.read_frame(16384).await.unwrap();
    fixture.commands.input(&unsigned("slow")).unwrap();
    fixture.commands.input(&unsigned("follow")).unwrap();
    wait_count(&fixture.service.waiting, 2).await;
    let origin = fixture.commands.origin.clone().unwrap();
    fixture.claim.operation_id = "moved".into();
    fixture.assignment.claim.as_mut().unwrap().operation_id = "moved".into();
    fixture.assignment.claim.as_mut().unwrap().delivery_generation = 2;
    let delivery = fixture.assignment.delivery.as_mut().unwrap();
    delivery.operation_id = "moved".into();
    delivery.owner_generation = 2;
    delivery.session.as_mut().unwrap().id = "new-session".into();
    *fixture.service.assignment.lock().unwrap() = fixture.assignment.clone();
    fixture.commands.bind(&fixture.claim, &fixture.assignment).unwrap();
    assert!(origin.cancellation.is_cancelled());
    assert!(fixture.commands.tasks.current(&origin, true).is_err()); // Configuration rejects effects.
    fixture.commands.tree(CommandTree::empty()).unwrap();
    fixture.commands.arrived();
    output(&mut fixture.commands, &mut public).await;
    client.read_frame(16384).await.unwrap();
    fixture.service.release.notify_waiters();
    output(&mut fixture.commands, &mut public).await;
    assert_eq!(VarInt::decode(&mut client.read_frame(16384).await.unwrap().as_ref()).unwrap().0, SystemMessage::ID);
    wait_count(&fixture.service.replies, 1).await;
    let platform = fixture.commands.tasks.platform.clone();
    platform.cleanup.close();
    tokio::time::timeout(Duration::from_secs(3), platform.cleanup.wait()).await.unwrap();
    assert_eq!(fixture.service.replies.load(Ordering::SeqCst), 1);
    assert!(fixture.commands.tasks.current(&origin, false).is_err());
    assert_eq!(fixture.commands.tasks.current(&origin, true).unwrap().scope.session_id, "new-session");
    assert!(origin.inspect(&fixture.commands.tasks.platform).await.is_err());
    fixture.close().await;
}

#[tokio::test]
async fn session_method_lost_start_polls_same_capture_and_never_retargets_after_move() {
    let mut fixture = Fixture::new().await;
    fixture.commands.arrived();
    let origin = fixture.commands.origin.clone().unwrap();
    let method = || effects::Method { app: "lobby".into(), session: "default".into(), name: "population".into() };
    let value =
        session::invoke(&fixture.commands.tasks, &origin, method(), serde_json::json!({}), false).await.unwrap();
    assert_eq!(value, serde_json::json!(42));
    assert_eq!(fixture.service.starts.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.service.polls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.service.methods.lock().unwrap()[0].claim.as_ref(), Some(&origin.identity));
    fixture.service.assignment.lock().unwrap().claim.as_mut().unwrap().delivery_generation += 1;
    assert!(session::invoke(&fixture.commands.tasks, &origin, method(), serde_json::json!({}), false).await.is_err());
    assert_eq!(fixture.service.methods.lock().unwrap().len(), 1);
    fixture.close().await;
}

#[tokio::test]
async fn accepted_session_send_outlives_handler_return_but_retains_captured_scope_cancellation() {
    let mut fixture = Fixture::new().await;
    fixture.commands.arrived();
    fixture.service.pending_method.store(true, Ordering::SeqCst);
    let origin = fixture.commands.origin.clone().unwrap();
    let method = effects::Method { app: "lobby".into(), session: "default".into(), name: "population".into() };
    session::invoke(&fixture.commands.tasks, &origin, method, serde_json::json!({}), true).await.unwrap();
    assert_eq!(fixture.commands.tasks.methods.available_permits(), 7);
    wait_count(&fixture.service.polls, 1).await;
    assert_eq!(fixture.service.cancels.load(Ordering::SeqCst), 0);
    fixture.commands.configuration();
    wait_count(&fixture.service.cancels, 1).await;
    assert_eq!(fixture.service.methods.lock().unwrap().len(), 1);
    assert_eq!(fixture.service.starts.load(Ordering::SeqCst), 1);
    fixture.close().await;
}

#[tokio::test]
async fn query_suggestions_use_owned_range_and_current_permission_and_platform_auth() {
    use chunk_protocol::{commands::CommandSuggestions, versions::v26_1::CommandSuggestionsRequest};
    let mut fixture = Fixture::new().await;
    let (client, public) = tokio::io::duplex(16384);
    let mut public = Transport::new(public);
    let mut client = Transport::new(client);
    fixture.commands.tree(CommandTree::empty()).unwrap();
    fixture.commands.arrived();
    output(&mut fixture.commands, &mut public).await;
    client.read_frame(16384).await.unwrap();
    let request = encode_packet(&CommandSuggestionsRequest {
        transaction_id: VarInt(19),
        text: McString::new("/travel al").unwrap(),
    })
    .unwrap();
    fixture.commands.input(body(&request)).unwrap();
    output(&mut fixture.commands, &mut public).await;
    let suggestions = decode_packet::<CommandSuggestions>(&client.read_frame(16384).await.unwrap()).unwrap();
    assert_eq!((suggestions.transaction_id, suggestions.start, suggestions.length), (19, 8, 2));
    assert_eq!(suggestions.matches.iter().map(McString::as_str).collect::<Vec<_>>(), ["alpha", "alpine"]);
    fixture.service.allowed.store(false, Ordering::SeqCst);
    fixture.commands.input(body(&request)).unwrap();
    output(&mut fixture.commands, &mut public).await;
    assert!(decode_packet::<CommandSuggestions>(&client.read_frame(16384).await.unwrap()).unwrap().matches.is_empty());
    let mut platform = fixture.commands.tasks.platform.clone();
    platform.target.backend.platform_token = Some("application".into());
    let scope = fixture.commands.origin.as_ref().unwrap().scope.clone();
    let response = backend::client(&platform).catalog(backend::authenticated(&platform, scope).unwrap()).await;
    assert_eq!(response.unwrap_err().code(), tonic::Code::Unauthenticated);
    fixture.close().await;
}

#[tokio::test]
async fn concurrent_effects_complete_independently_and_accept_unique_out_of_order_sequences() {
    let mut fixture = Fixture::new().await;
    fixture.commands.arrived();
    fixture.service.pending_method.store(true, Ordering::SeqCst);
    let tasks = fixture.commands.tasks.clone();
    let origin = fixture.commands.origin.clone().unwrap();
    let (client, public) = tokio::io::duplex(16384);
    let mut public = Transport::new(public);
    let mut client = Transport::new(client);
    // The initial catalog result does not emit a tree until the JVM publishes one.
    output(&mut fixture.commands, &mut public).await;
    let run = tokio::spawn(async move { run::execute(&tasks, &origin, false, "parallel").await });
    let event = tokio::time::timeout(Duration::from_secs(3), fixture.commands.receive()).await.unwrap();
    assert_eq!(fixture.service.replies.load(Ordering::SeqCst), 0); // No receipt before the actual write.
    fixture.commands.publish(event, &mut public).await.unwrap();
    assert_eq!(VarInt::decode(&mut client.read_frame(16384).await.unwrap().as_ref()).unwrap().0, SystemMessage::ID);
    wait_count(&fixture.service.replies, 1).await;
    assert_eq!(*fixture.service.reply_order.lock().unwrap(), [2]);
    assert!(!run.is_finished());
    fixture.service.pending_method.store(false, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(3), run).await.unwrap().unwrap().unwrap();
    assert_eq!(*fixture.service.reply_order.lock().unwrap(), [2, 4]);
    fixture.close().await;
}

#[tokio::test]
async fn accepted_sends_survive_success_but_abort_with_failed_invocation() {
    for succeeded in [true, false] {
        let mut fixture = Fixture::new().await;
        fixture.commands.arrived();
        fixture.service.pending_method.store(true, Ordering::SeqCst);
        let origin = fixture.commands.origin.clone().unwrap();
        let result =
            run::execute(&fixture.commands.tasks, &origin, false, if succeeded { "send_ok" } else { "send_fail" })
                .await;
        assert_eq!(result.is_ok(), succeeded);
        if succeeded {
            wait_count(&fixture.service.polls, 1).await;
            assert_eq!(fixture.service.cancels.load(Ordering::SeqCst), 0);
            fixture.commands.configuration();
        }
        wait_count(&fixture.service.cancels, 1).await;
        assert_eq!(fixture.service.methods.lock().unwrap().len(), 1);
        fixture.close().await;
    }
}
