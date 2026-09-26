use super::*;
use chunk_protocol::{
    Decode, Encode, McString, Packet, VarInt,
    commands::{CommandNode, NodeKind, SignedCommand, SystemMessage},
    decode_packet,
    versions::v26_2::{ConfigurationClientInformation, ConfigurationClientInformationParticleStatus, UnsignedCommand},
};
use fixture::Fixture;
use std::{sync::atomic::Ordering, time::Duration};
use tokio::io::DuplexStream;

#[tokio::test]
async fn failed_move_stops_preparation_and_reports_the_reason_despite_a_lost_report() {
    use chunk_proto::sync::v1::{Error, GatewayMove, SessionDemand, error::Code};
    for (code, message) in [
        (Code::Invalid, "destination configuration rejected"),
        (Code::Stopped, "runtime stopped"),
        (Code::Unavailable, "app did not become ready within 35 seconds"),
    ] {
        let transient = code == Code::Unavailable;
        let fixture = Fixture::new().await;
        let source = super::super::ClaimGuard {
            platform: fixture.commands.tasks.platform.clone(),
            claim: fixture.claim.clone(),
            armed: false,
            failure: None,
        };
        let identity = fixture.identity.clone();
        let destination = GatewayMove {
            operation_id: "failed-move".into(),
            destination: Some(SessionDemand {
                key: "lobby".into(),
                session_type: "lobby/default".into(),
                machine_profile: "local".into(),
            }),
        };
        let movement = fixture.service.movement.clone();
        {
            let mut state = movement.lock().unwrap();
            state.pending = Some(destination);
            state.error = Some(Error { code: code.into(), message: message.into() });
            state.lose_report = true;
            state.stall_retries = transient;
        }
        fixture.service.publish();
        let running = tokio::spawn(async move { super::super::next_move(&source, &identity, 776).await.map(|_| ()) });
        if transient {
            tokio::time::timeout(Duration::from_secs(3), async {
                while movement.lock().unwrap().attempts < 2 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(movement.lock().unwrap().reports, 0);
            tokio::time::pause();
            tokio::time::advance(super::super::WAIT_TIMEOUT).await;
            tokio::time::resume();
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            while movement.lock().unwrap().failure.is_none() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        {
            let state = movement.lock().unwrap();
            assert_eq!(state.attempts, if transient { 2 } else { 1 });
            assert_eq!(state.reports, 2);
            assert!(state.pending.is_none());
            let (operation, reason) = state.failure.as_ref().unwrap();
            assert_eq!(operation, "failed-move");
            if transient {
                assert_eq!(*reason, format!("move preparation timed out after 45 seconds: {message}"));
            } else {
                assert_eq!(reason, message);
            }
        }
        assert!(!running.is_finished()); // Keeps waiting for another move on the same source.
        running.abort();
        assert!(running.await.unwrap_err().is_cancelled());
        fixture.close().await;
    }
}

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
    fixture.commands.tree(&CommandTree::empty()).unwrap();
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
    let hidden = fixture.commands.tree(&CommandTree::empty()).unwrap();
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
    let forwarded = decode_packet::<CommandTree>(body(&fixture.commands.tree(&collision).unwrap())).unwrap();
    assert_eq!(forwarded.nodes[0].children, vec![1]);
    assert!(!fixture.commands.input(&unsigned("echo")).unwrap());
    fixture.close().await;
}

#[tokio::test]
async fn failed_permission_refresh_keeps_last_catalog() {
    let mut fixture = Fixture::new().await;
    let (client, public) = tokio::io::duplex(16384);
    let mut public = Transport::new(public);
    let mut client = Transport::new(client);
    fixture.commands.tree(&CommandTree::empty()).unwrap();
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
    fixture.commands.tree(&CommandTree::empty()).unwrap();
    fixture.commands.arrived();
    output(&mut fixture.commands, &mut public).await;
    client.read_frame(16384).await.unwrap();
    fixture.commands.input(&unsigned("slow")).unwrap();
    fixture.commands.input(&unsigned("follow")).unwrap();
    wait_count(&fixture.service.waiting, 2).await;
    let origin = fixture.commands.origin.clone().unwrap();
    let moved = fixture::Held {
        operation: "moved".into(),
        generation: chunk_proto::sync::v1::Position { epoch: 1, revision: 2 },
        phase: chunk_proto::sync::v1::ClaimPhase::Arrived,
    };
    fixture.claim.operation_id = "moved".into();
    fixture.identity.operation_id = "moved".into();
    fixture.identity.delivery_generation = crate::server::platform::generation(&moved.generation);
    fixture.session = "new-session".into();
    *fixture.service.claim.lock().unwrap() = moved;
    fixture.sync().await;
    fixture.commands.bind(&fixture.claim, &fixture.identity, &fixture.session).unwrap();
    assert!(origin.cancellation.is_cancelled());
    assert!(fixture.commands.tasks.current(&origin, true).is_err()); // Configuration rejects effects.
    fixture.commands.tree(&CommandTree::empty()).unwrap();
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
    assert!(origin.check(&fixture.commands.tasks.platform).await.is_err());
    fixture.close().await;
}

#[tokio::test]
async fn claim_view_resyncs_from_a_snapshot_after_its_stream_drops() {
    let fixture = Fixture::new().await;
    let origin = fixture.commands.origin.clone().unwrap();
    let platform = fixture.commands.tasks.platform.clone();
    fixture.sync().await;
    origin.check(&platform).await.unwrap();
    fixture.service.watch_down.store(true, Ordering::SeqCst);
    fixture.service.claim.lock().unwrap().phase = chunk_proto::sync::v1::ClaimPhase::Withdrawing;
    fixture.service.publish();
    wait_count(&fixture.service.refused_watches, 1).await;
    // A stale view answers nothing, so no command acts on the claim's last known phase.
    assert!(tokio::time::timeout(Duration::from_millis(100), platform.claims(|_| Some(()))).await.is_err());
    fixture.service.watch_down.store(false, Ordering::SeqCst);
    fixture.sync().await;
    assert!(origin.check(&platform).await.is_err());
    fixture.close().await;
}

#[tokio::test]
async fn a_call_on_a_superseded_stream_runs_again_on_its_replacement_under_the_same_operation() {
    use chunk_proto::sync::v1::WithdrawResult;
    let fixture = Fixture::new().await;
    let platform = fixture.commands.tasks.platform.clone();
    fixture.sync().await;
    // Core drops the stream, then stops the call once the gateway resubscribes, before the new stream's first update.
    fixture.service.supersede.store(true, Ordering::SeqCst);
    let withdrawal = platform.call::<WithdrawResult>("withdraw", "claim", &(), crate::server::platform::RPC_TIMEOUT);
    tokio::time::timeout(Duration::from_secs(3), withdrawal).await.unwrap().unwrap();
    // Only the retry on the replacement stream reached the claim.
    assert_eq!(fixture.service.logins.lock().unwrap().cancels, ["claim"]);
    fixture.sync().await;
    fixture.close().await;
}

#[tokio::test]
async fn session_method_lost_start_polls_same_capture_and_never_retargets_after_move() {
    let mut fixture = Fixture::new().await;
    fixture.commands.arrived();
    let origin = fixture.commands.origin.clone().unwrap();
    let method =
        || chunk_contract::EffectMethod { app: "lobby".into(), session: "default".into(), name: "population".into() };
    let value =
        session::invoke(&fixture.commands.tasks, &origin, method(), serde_json::json!({}), false).await.unwrap();
    assert_eq!(value, serde_json::json!(42));
    assert_eq!(fixture.service.starts.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.service.polls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.service.methods.lock().unwrap()[0].claim.as_ref(), Some(&origin.identity));
    fixture.service.claim.lock().unwrap().generation.revision += 1;
    fixture.sync().await;
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
    let method =
        chunk_contract::EffectMethod { app: "lobby".into(), session: "default".into(), name: "population".into() };
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
    use chunk_protocol::{commands::CommandSuggestions, versions::v26_2::CommandSuggestionsRequest};
    let mut fixture = Fixture::new().await;
    let (client, public) = tokio::io::duplex(16384);
    let mut public = Transport::new(public);
    let mut client = Transport::new(client);
    fixture.commands.tree(&CommandTree::empty()).unwrap();
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

#[tokio::test]
async fn roster_activation_waits_for_the_group_and_other_failures_still_end_the_connection() {
    let fixture = Fixture::new().await;
    let guard = crate::server::managed::ClaimGuard {
        platform: fixture.commands.tasks.platform.clone(),
        claim: fixture.claim.clone(),
        armed: false,
        failure: None,
    };
    fixture.service.roster_waits.store(3, Ordering::SeqCst);
    crate::server::managed::activate(&guard).await.unwrap();
    assert_eq!(fixture.service.activations.load(Ordering::SeqCst), 4);
    let mut target = fixture.commands.tasks.platform.target.clone();
    target.gateway.credential = "wrong".into();
    let unauthorized = crate::server::managed::ClaimGuard {
        platform: crate::server::platform::Platform::new(target).unwrap(),
        claim: fixture.claim.clone(),
        armed: false,
        failure: None,
    };
    assert!(crate::server::managed::activate(&unauthorized).await.is_err());
    assert_eq!(fixture.service.activations.load(Ordering::SeqCst), 4);
    fixture.close().await;
}

#[tokio::test]
async fn a_login_routed_again_keeps_its_operation_until_the_connection_deadline_cancels_its_reservation() {
    let fixture = Fixture::new().await;
    let logins = &fixture.service.logins;
    logins.lock().unwrap().retired = Some("deployment".into());
    let current = crate::server::Retarget(Arc::new(std::sync::RwLock::new(fixture.commands.tasks.platform.clone())));
    let (_client, public) = tokio::io::duplex(16384);
    let authenticated = crate::server::authentication::Authenticated {
        protocol_version: 776,
        profile: chunk_protocol::versions::v26_2::LoginSuccess {
            session_id: chunk_protocol::Uuid([2; 16]),
            uuid: chunk_protocol::Uuid([1; 16]),
            username: McString::new("Alex").unwrap(),
            properties: chunk_protocol::BoundedArray::new(vec![]).unwrap(),
        },
        transport: Transport::new(public),
    };
    // After core refuses the login, its next routing fails once the release it used is no longer current.
    let reload = async {
        let (fail, unroutable) = tokio::sync::oneshot::channel();
        while logins.lock().unwrap().claims.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        logins.lock().unwrap().unroutable = Some(unroutable);
        while logins.lock().unwrap().unroutable.is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut target = current.platform().target;
        target.backend.deployment = "next".into();
        current.replace(target).unwrap();
        fail.send(()).unwrap();
    };
    // The client never finishes configuration, so the connection's own deadline ends the login.
    let deadline = Duration::from_secs(1);
    let started = tokio::time::Instant::now();
    let (served, ()) = tokio::join!(super::super::serve(authenticated, &current, deadline), reload);
    assert_eq!(served.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    assert!((deadline..deadline * 2).contains(&started.elapsed()));
    while logins.lock().unwrap().cancels.is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (claims, cancels) = {
        let logins = logins.lock().unwrap();
        (logins.claims.clone(), logins.cancels.clone())
    };
    let (reserved, rejected) = claims.split_last().unwrap();
    assert_eq!(reserved.1.deployment, "next");
    assert!(!rejected.is_empty() && rejected.iter().all(|(_, login)| login.deployment == "deployment"));
    assert!(
        claims
            .iter()
            .all(|(operation, login)| *operation == reserved.0 && login.connection_id == reserved.1.connection_id)
    );
    // Rejected attempts reserved nothing; only the reservation the deadline abandoned is withdrawn.
    assert_eq!(cancels, std::slice::from_ref(&reserved.0));
    fixture.close().await;
}
