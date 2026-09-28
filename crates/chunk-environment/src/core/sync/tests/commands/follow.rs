//! A `follow_player` command that moves its player: the gateway cuts the player over, and the command's later effects
//! follow them to their new claim.

use super::*;
use chunk_proto::sync::v1::{ActivateResult, ClaimArguments, ClaimResult, claim_result};

/// Reads the gateway's topic until the player's `login` claim shows a pending move, returning its operation ID.
async fn pending_move(updates: &mut Streaming<Update>) -> String {
    loop {
        let update = next(updates).await;
        let pending = update.upserts.iter().find_map(|entry| match &entry.state {
            Some(State::Value(value)) if entry.key == "login" => GatewayClaim::decode(&value[..]).unwrap().pending_move,
            _ => None,
        });
        if let Some(pending) = pending {
            assert_eq!(pending.destination.unwrap().key, "arena");
            return pending.operation_id;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_follow_player_commands_move_cuts_the_player_over_and_its_later_effects_follow_them() {
    let Arrived { mut fixture, mut updates, gateway, jvm } = arrived().await;
    let (credential, stream) = (gateway.credential.clone(), gateway.stream.clone());
    let operation = gateway.prepare().await;
    decoded::<CommandStarted>(&gateway.start(&operation, FOLLOW, "follow cutover", runtime::PLAYER).await);
    let mut effects = gateway.follow(&operation).await;

    // The gateway claims the move, releases the source, then activates the destination, which the JVM arrives.
    let moved = pending_move(&mut updates).await;
    let claimed = fixture.platform(&credential, &stream, &moved, "chunk:claim", &ClaimArguments::default()).await;
    assert!(matches!(decoded::<ClaimResult>(&claimed).outcome, Some(claim_result::Outcome::Assignment(_))));
    let withdrawn = fixture.platform(&credential, &stream, "login", "chunk:withdraw", &()).await;
    assert!(!decoded::<WithdrawResult>(&withdrawn).unknown);
    while !next(&mut updates).await.removed.iter().any(|key| key == "login") {}
    let activated = fixture.platform(&credential, &stream, &moved, "chunk:activate", &()).await;
    assert!(!decoded::<ActivateResult>(&activated).waiting);
    claims::arrival(&mut updates, &moved).await;

    // The handler messages the player once the count is set; the message reaches them on their new claim.
    let cli = fixture.cli.clone();
    assert_eq!(fixture.call(&cli, "cutover", "add", "1").await.outcome, Some(Outcome::Result(b"1".to_vec())));
    let (key, value) = loop {
        let update = next(&mut effects).await;
        assert!(update.upserts.iter().all(|entry| entry.key != "outcome"), "the command ended first: {update:?}");
        if let Some(Entry { key, state: Some(State::Value(value)) }) = update.upserts.into_iter().next() {
            break (key, value);
        }
    };
    let effect = CommandEffect::decode(&value[..]).unwrap().effect;
    assert_eq!(effect, Some(command_effect::Effect::Message("followed".into())));
    let ack = EffectArguments { operation_id: operation, sequence: key.parse().unwrap(), failed: false };
    assert!(!decoded::<EffectResult>(&gateway.call("", "chunk:effect", &ack).await).unknown);
    assert_eq!(returned(&outcome(&mut effects).await), b"null");
    Arrived { fixture, updates, gateway, jvm }.stop().await;
}
