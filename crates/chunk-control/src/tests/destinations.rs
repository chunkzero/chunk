use super::*;

fn policy(fixture: &mut Fixture, overflow: &str) {
    fixture.config.destinations = Some(
        serde_json::from_value(serde_json::json!({
            "version":1,"entries":{"shared/destinations/lobby":{
                "destination":{"key":"lobby","session_type":"bridge/default","machine_profile":"local"},
                "overflow":overflow,"empty_timeout_seconds":1
            }}
        }))
        .unwrap(),
    );
}

#[tokio::test]
async fn declared_pools_coalesce_concurrent_demand_and_pin_policy_and_version() {
    for (overflow, accepted, sessions) in [("reject", 2, 1), ("replicate", 4, 2)] {
        let mut fixture = Fixture::new().await;
        policy(&mut fixture, overflow);
        let control = fixture.control();
        let mut calls = tokio::task::JoinSet::new();
        for index in 0..4 {
            let control = control.clone();
            calls.spawn(async move {
                control.claim(request(&format!("claim-{index}"), &uuid::Uuid::new_v4().to_string())).await
            });
        }
        let mut count = 0;
        while let Some(result) = calls.join_next().await {
            match result.unwrap() {
                Ok(_) => count += 1,
                Err(Error::Capacity) => {}
                Err(error) => panic!("{error}"),
            }
        }
        assert_eq!(count, accepted);
        assert_eq!(fixture.runtime.sessions.lock().unwrap().len(), sessions);
        assert_eq!(control.state().unwrap().claims.len(), accepted);
        let mut wrong = request("wrong-profile", &uuid::Uuid::new_v4().to_string());
        wrong.demand.as_mut().unwrap().machine_profile.clear();
        assert!(control.claim(wrong).await.is_err());
        drop(control);
        for change in ["version", "profile", "timeout"] {
            let mut config = fixture.config.clone();
            match change {
                "version" => config.deployment.deployment = "next".into(),
                "profile" => {
                    config
                        .destinations
                        .as_mut()
                        .unwrap()
                        .entries
                        .values_mut()
                        .next()
                        .unwrap()
                        .destination
                        .machine_profile = "changed".into();
                }
                _ => {
                    config.destinations.as_mut().unwrap().entries.values_mut().next().unwrap().empty_timeout_seconds =
                        2;
                }
            }
            assert!(
                Control::open(&fixture.directory.path().join("control.sqlite"), config, fixture.host.clone()).is_err()
            );
        }
        fixture.close().await;
    }
}

#[tokio::test]
async fn lost_creation_and_preparation_recover_the_original_reservation() {
    let mut fixture = Fixture::new().await;
    policy(&mut fixture, "reject");
    let control = fixture.control();
    let claim = request("lost", &uuid::Uuid::new_v4().to_string());
    fixture.runtime.lost_creation.store(true, Ordering::Release);
    assert!(control.claim(claim.clone()).await.is_err());
    let original = control.state().unwrap().claims["lost"].clone();
    fixture.runtime.lost_preparation.store(true, Ordering::Release);
    assert!(control.claim(claim.clone()).await.is_err());
    let recovered = control.claim(claim.clone()).await.unwrap();
    assert_eq!(recovered.claim.unwrap(), original.identity("lost"));
    assert_eq!(fixture.runtime.sessions.lock().unwrap().len(), 1);
    assert_eq!(fixture.runtime.bindings.lock().unwrap().len(), 1);
    control.cancel(claim.clone()).await.unwrap();
    assert!(control.claim(claim).await.is_err());
    assert_eq!(fixture.runtime.sessions.lock().unwrap().len(), 1);
    fixture.close().await;
}

#[tokio::test]
async fn empty_expiry_counts_reservations_and_waits_for_lost_finish_reconciliation() {
    let mut fixture = Fixture::new().await;
    policy(&mut fixture, "reject");
    let control = fixture.control();
    let claim = request("reserved", &uuid::Uuid::new_v4().to_string());
    control.claim(claim.clone()).await.unwrap();
    let session = control.state().unwrap().claims["reserved"].session.clone();
    control
        .update(|state| {
            state.sessions.get_mut(&session).unwrap().empty_since_ms = Some(0);
            Ok(())
        })
        .unwrap();
    control.reconcile_all().await.unwrap();
    assert_eq!(fixture.runtime.finishes.load(Ordering::Acquire), 0);
    assert!(control.state().unwrap().sessions[&session].empty_since_ms.is_none());
    control.cancel(claim).await.unwrap();
    control
        .update(|state| {
            state.sessions.get_mut(&session).unwrap().empty_since_ms = Some(0);
            Ok(())
        })
        .unwrap();
    fixture.runtime.lost_finish.store(true, Ordering::Release);
    control.reconcile_all().await.unwrap();
    let state = control.state().unwrap();
    assert!(state.sessions[&session].retired);
    assert!(!state.sessions[&session].finished);
    assert!(matches!(control.claim(request("early", &uuid::Uuid::new_v4().to_string())).await, Err(Error::Capacity)));
    control.reconcile_all().await.unwrap();
    assert!(control.state().unwrap().sessions[&session].finished);
    let next = control.claim(request("next", &uuid::Uuid::new_v4().to_string())).await.unwrap();
    assert_ne!(next.delivery.unwrap().session.unwrap().id, session);
    assert_eq!(fixture.runtime.finishes.load(Ordering::Acquire), 1);
    fixture.close().await;
}

#[tokio::test]
async fn failed_unknown_creation_is_retained_until_host_fencing_and_finish_requires_current_claim() {
    let mut fixture = Fixture::new().await;
    policy(&mut fixture, "reject");
    let control = fixture.control();
    let claim = request("failed", &uuid::Uuid::new_v4().to_string());
    fixture.runtime.failed_creation.store(true, Ordering::Release);
    assert!(control.claim(claim.clone()).await.is_err());
    control.cancel(claim).await.unwrap();
    let session = control.state().unwrap().claims["failed"].session.clone();
    control
        .update(|state| {
            state.sessions.get_mut(&session).unwrap().empty_since_ms = Some(0);
            Ok(())
        })
        .unwrap();
    control.reconcile_all().await.unwrap();
    assert!(!control.state().unwrap().sessions[&session].finished);
    assert!(matches!(
        control.claim(request("duplicate", &uuid::Uuid::new_v4().to_string())).await,
        Err(Error::Capacity)
    ));
    fixture.runtime.stopped.store(true, Ordering::Release);
    control.reconcile_all().await.unwrap();
    assert!(control.state().unwrap().sessions[&session].finished);
    fixture.close().await;

    let fixture = Fixture::new().await;
    let control = fixture.control();
    let claim = request("arrived", &uuid::Uuid::new_v4().to_string());
    let identity = control.claim(claim.clone()).await.unwrap().claim.unwrap();
    assert!(control.finish_destination(&identity).is_err());
    fixture.runtime.bindings.lock().unwrap().get_mut("arrived").unwrap().phase = DeliveryPhase::Arrived;
    control.activate(ActivateClaim { claim: Some(identity.clone()) }).await.unwrap();
    let mut stale = identity.clone();
    stale.delivery_generation += 1;
    assert!(control.finish_destination(&stale).is_err());
    control.finish_destination(&identity).unwrap();
    control.reconcile_all().await.unwrap();
    control.reconcile_all().await.unwrap();
    assert!(control.state().unwrap().sessions.values().all(|session| session.finished));
    assert!(control.finish_destination(&identity).is_err());
    fixture.close().await;
}
