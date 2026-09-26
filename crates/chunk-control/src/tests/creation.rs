use super::*;
use serde_json::json;

fn configured_destinations(fixture: &mut Fixture) {
    fixture.config.contracts.session_configurations = Some(
        serde_json::from_value(json!({
            "version":1,"configurations":[{"app":"bridge","session":"default","configuration":{
                "type":"object","fields":{"map":{"schema":{"type":"enum","values":["forest","desert"]}}}
            }}]
        }))
        .unwrap(),
    );
    fixture.config.contracts.destinations = Some(serde_json::from_value(json!({
        "version":1,"entries":{
            "apps/bridge/destinations/small":{
                "destination":{"key":"small","session_type":"bridge/default","machine_profile":"local"},
                "overflow":"reject","empty_timeout_seconds":60,"creation":{"capacity":1,"configuration":{"map":"forest"}}
            },
            "apps/bridge/destinations/large":{
                "destination":{"key":"large","session_type":"bridge/default","machine_profile":"local"},
                "overflow":"reject","empty_timeout_seconds":60,"creation":{"capacity":3,"configuration":{"map":"desert"}}
            }
        }
    })).unwrap());
}

fn demand(operation: &str, key: &str) -> ClaimRequest {
    let mut claim = request(operation, &uuid::Uuid::new_v4().to_string());
    claim.demand.as_mut().unwrap().key = key.into();
    claim
}

#[tokio::test]
async fn one_implementation_uses_frozen_destination_capacity_and_configuration_across_recovery() {
    let mut fixture = Fixture::new().await;
    configured_destinations(&mut fixture);
    let control = fixture.control();
    let small = demand("small", "small");
    fixture.runtime.lost_creation.store(true, Ordering::Release);
    assert!(control.claim(small.clone()).await.is_err());
    let reserved = control.state().unwrap().claims["small"].session.clone();
    drop(control);
    let control = fixture.control();
    let recovered = control.claim(small).await.unwrap();
    assert_eq!(recovered.delivery.unwrap().session.unwrap().id, reserved);
    for index in 0..3 {
        control.claim(demand(&format!("large-{index}"), "large")).await.unwrap();
    }
    assert!(matches!(control.claim(demand("small-full", "small")).await, Err(Error::Capacity)));
    assert!(matches!(control.claim(demand("large-full", "large")).await, Err(Error::Capacity)));
    let state = control.state().unwrap();
    assert_eq!(state.sessions.len(), 2);
    assert_eq!(state.hosts.len(), 1);
    let large = &state.sessions[&state.claims["large-0"].session];
    assert_eq!(large.capacity, 3);
    assert_eq!(large.configuration, json!({"map":"desert"}));
    assert_eq!(state.sessions[&reserved].capacity, 1);
    assert_eq!(state.sessions[&reserved].configuration, json!({"map":"forest"}));
    assert!(state.sessions.values().all(|session| session.session_type == "bridge/default"));
    for index in 1..3 {
        assert_eq!(state.claims[&format!("large-{index}")].session, state.claims["large-0"].session);
    }
    let commands = fixture.runtime.sessions.lock().unwrap().clone();
    assert_eq!(commands.len(), 2);
    for (id, session) in &state.sessions {
        assert_eq!(commands[id].capacity, session.capacity);
        assert_eq!(commands[id].configuration_json, serde_json::to_vec(&session.configuration).unwrap());
    }
    drop(control);
    let mut changed = fixture.config.clone();
    changed
        .contracts
        .destinations
        .as_mut()
        .unwrap()
        .entries
        .get_mut("apps/bridge/destinations/small")
        .unwrap()
        .creation
        .as_mut()
        .unwrap()
        .configuration = json!({"map":"desert"});
    assert!(open(&fixture.directory.path().join("control.sqlite"), changed, fixture.host.clone()).is_err());
    fixture.close().await;
}

#[tokio::test]
async fn malformed_or_undeclared_creation_is_rejected_before_reservation_or_launch() {
    let mut fixture = Fixture::new().await;
    configured_destinations(&mut fixture);
    for configuration in [json!({}), json!({"map":"ocean"}), json!({"map":"forest","extra":true}), json!([])] {
        let mut config = fixture.config.clone();
        config
            .contracts
            .destinations
            .as_mut()
            .unwrap()
            .entries
            .values_mut()
            .next()
            .unwrap()
            .creation
            .as_mut()
            .unwrap()
            .configuration = configuration;
        assert!(open(&fixture.directory.path().join("invalid.sqlite"), config, fixture.host.clone()).is_err());
    }
    for field in ["schema", "profile", "capacity"] {
        let mut config = fixture.config.clone();
        match field {
            "schema" => config.contracts.session_configurations = None,
            "profile" => {
                config
                    .contracts
                    .destinations
                    .as_mut()
                    .unwrap()
                    .entries
                    .values_mut()
                    .next()
                    .unwrap()
                    .destination
                    .machine_profile = "unknown".into();
            }
            _ => {
                config
                    .contracts
                    .destinations
                    .as_mut()
                    .unwrap()
                    .entries
                    .values_mut()
                    .next()
                    .unwrap()
                    .creation
                    .as_mut()
                    .unwrap()
                    .capacity = 0;
            }
        }
        assert!(open(&fixture.directory.path().join("invalid.sqlite"), config, fixture.host.clone()).is_err());
    }
    let control = fixture.control();
    assert!(control.claim(demand("missing-configuration", "undeclared")).await.is_err());
    assert!(control.state().unwrap().claims.is_empty());
    assert!(fixture.host.ids.lock().unwrap().is_empty());
    let source = demand("source", "small");
    let assignment = control.claim(source.clone()).await.unwrap();
    fixture.runtime.bindings.lock().unwrap().get_mut("source").unwrap().phase = DeliveryPhase::Arrived;
    control.activate(ActivateClaim { claim: assignment.claim }).await.unwrap();
    assert!(matches!(
        control.move_player(chunk_proto::v1::MovePlayerRequest {
            operation_id: "invalid-move".into(),
            player_id: source.identity.as_ref().unwrap().uuid.clone(),
            demand: Some(SessionDemand { key: "undeclared".into(), ..source.demand.clone().unwrap() }),
            ..Default::default()
        }),
        Err(Error::Invalid("session configuration differs from its implementation schema"))
    ));
    assert!(control.state().unwrap().moves.is_empty());
    assert!(control.poll_move(&source).unwrap().claim.is_none());
    fixture.close().await;
}
