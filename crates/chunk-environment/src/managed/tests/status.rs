//! Status reports against the fake management.

use super::*;
use crate::{
    Core,
    managed::{
        ATTACH_IDLE, Lease, Managed, REATTACH,
        status::{INTERVAL, OBSERVE, Observed, PrivateAddress, Reporter},
    },
};
use std::{
    net::{IpAddr, SocketAddr},
    sync::OnceLock,
};

/// Keeps paused time from advancing on its own while the returned sender lives, however long real work takes.
pub(super) fn hold_time() -> std::sync::mpsc::Sender<()> {
    let (release, held) = std::sync::mpsc::channel::<()>();
    tokio::task::spawn_blocking(move || held.recv());
    release
}

/// Advances held time a second at a time for `limit`, letting real work settle between steps.
pub(super) async fn advance_for(limit: Duration) {
    for _ in 0..limit.as_secs() {
        tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(20))).await.unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_gateway_is_reported_at_the_address_management_is_reached_from_once_it_starts() {
    let mut harness = Harness::new().await;
    harness.deploy("dep_a", harness.valid());
    let gateway = GatewayConfig::new("0.0.0.0:0".parse().unwrap());
    let (stop, running) = harness.start_with(gateway, crate::RELEASE_TIMEOUT);
    let loading = harness.expect(1, "dep_a", DeploymentState::InProgress).await;
    assert!(loading.gateway_addresses.is_empty());
    let active = harness.expect(1, "dep_a", DeploymentState::Active).await;
    let [address] = active.gateway_addresses.as_slice() else { panic!("{:?}", active.gateway_addresses) };
    let address: SocketAddr = address.parse().unwrap();
    assert!(address.ip().is_loopback() && address.port() != 0, "{address}");

    stop.cancel();
    tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn a_gateway_on_every_interface_is_reported_only_at_an_address_of_the_family_it_accepts() {
    let reached = async |configured: &str, bound: &str| {
        let configured: IpAddr = configured.parse().unwrap();
        let address = PrivateAddress::new(Some(configured), "http://127.0.0.1:1").gateway(bound.parse().unwrap()).await;
        address.map(|address| address.to_string())
    };
    assert_eq!(reached("10.0.0.2", "0.0.0.0:25565").await.as_deref(), Some("10.0.0.2:25565"));
    assert_eq!(reached("::ffff:10.0.0.2", "[::ffff:0.0.0.0]:25565").await.as_deref(), Some("10.0.0.2:25565"));
    assert_eq!(reached("fd00::2", "[::]:25565").await.as_deref(), Some("[fd00::2]:25565"));
    assert_eq!(reached("fd00::2", "0.0.0.0:25565").await, None);
    assert_eq!(reached("10.0.0.2", "[::]:25565").await, None);
    assert_eq!(reached("10.0.0.2", "[::ffff:10.0.0.3]:25565").await.as_deref(), Some("10.0.0.3:25565"));
}

/// Advances paused time by `ticks` of [`OBSERVE`], each once the reporter observed the tick before, and returns the
/// reports management applied since. Each observation follows the reports sent before it, so these are the reports sent
/// before the last tick, and any sent on it.
async fn tick(
    ticks: u32,
    observations: &mut mpsc::UnboundedReceiver<()>,
    reported: &mut mpsc::UnboundedReceiver<ReportStatusRequest>,
) -> Vec<(u64, u64, Vec<String>)> {
    for _ in 0..ticks {
        tokio::time::advance(OBSERVE).await;
        observations.recv().await.unwrap();
    }
    std::iter::from_fn(|| reported.try_recv().ok())
        .map(|report| (report.lease, report.sequence, report.gateway_addresses))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn reports_repeat_under_one_lease_a_changed_address_reports_at_once_and_a_new_lease_starts_over() {
    let mut harness = Harness::new().await;
    let _time = hold_time();
    let reporter = Reporter::new(harness.management_config().client(), CancellationToken::new());
    let observed = Mutex::new(Some(Observed { lease: 0, revision: 1, ..Observed::default() }));
    let (observing, mut observations) = mpsc::unbounded_channel();
    let observe = || {
        observing.send(()).unwrap();
        std::future::ready(observed.lock().unwrap().clone())
    };
    let reporting = reporter.keep_reporting(observe, |_| false);
    let checks = async {
        let applied = &mut harness.reported;
        let interval = u32::try_from(INTERVAL.as_secs() / OBSERVE.as_secs()).unwrap();
        observations.recv().await.unwrap();
        let first = applied.recv().await.unwrap();
        assert_eq!((first.lease, first.sequence, first.desired_revision), (0, 1, 1));
        assert!(first.gateway_addresses.is_empty() && first.deployment.is_none() && !first.ready_to_suspend);
        // No tick a report is due on ends a `tick` call, so each call's reports are exact.
        assert_eq!(tick(interval - 1, &mut observations, applied).await, []);
        assert_eq!(tick(2, &mut observations, applied).await, [(0, 2, vec![])]);
        assert_eq!(tick(interval - 2, &mut observations, applied).await, []);
        assert_eq!(tick(2, &mut observations, applied).await, [(0, 3, vec![])]);

        let address = vec!["10.0.0.2:25565".to_owned()];
        observed.lock().unwrap().as_mut().unwrap().gateway_addresses.clone_from(&address);
        assert_eq!(tick(2, &mut observations, applied).await, [(0, 4, address.clone())]);

        // Under the next attach's lease, the sequence starts over.
        harness.management.lease.send_replace(1);
        observed.lock().unwrap().as_mut().unwrap().lease = 1;
        assert_eq!(tick(interval - 2, &mut observations, applied).await, []);
        assert_eq!(tick(2, &mut observations, applied).await, [(1, 1, address)]);

        // Superseded or stopping, core reports nothing more.
        *observed.lock().unwrap() = None;
        assert_eq!(tick(interval * 3, &mut observations, applied).await, []);
    };
    tokio::select! {
        error = reporting => panic!("{error}"),
        () = checks => {}
    }
}

#[tokio::test(start_paused = true)]
async fn a_change_management_did_not_accept_is_sent_again_with_backoff_until_it_is() {
    let mut harness = Harness::new().await;
    let _time = hold_time();
    let reporter = Reporter::new(harness.management_config().client(), CancellationToken::new());
    let observed = Mutex::new(Observed { lease: 0, revision: 1, ready_to_suspend: true, ..Observed::default() });
    let (observing, mut observations) = mpsc::unbounded_channel();
    let observe = || {
        observing.send(()).unwrap();
        std::future::ready(Some(observed.lock().unwrap().clone()))
    };
    let reporting = reporter.keep_reporting(observe, |_| false);
    let checks = async {
        let applied = &mut harness.reported;
        observations.recv().await.unwrap();
        assert!(applied.recv().await.unwrap().ready_to_suspend);

        // Readiness is revoked and the address changes, but management refuses the next three reports.
        *harness.management.unavailable_reports.lock().unwrap() = 3;
        let address = vec!["10.0.0.2:25565".to_owned()];
        {
            let mut observed = observed.lock().unwrap();
            observed.ready_to_suspend = false;
            observed.gateway_addresses.clone_from(&address);
        }
        let mut ticks = 0;
        let report = loop {
            tokio::time::advance(OBSERVE).await;
            observations.recv().await.unwrap();
            ticks += 1;
            if let Ok(report) = applied.try_recv() {
                break report;
            }
            assert!(ticks < 10, "the change was not sent again");
        };
        // Sent at once, then again after one, two and four seconds, each under a new sequence.
        assert_eq!((report.sequence, report.ready_to_suspend, report.gateway_addresses), (5, false, address));
        assert_eq!(*harness.management.unavailable_reports.lock().unwrap(), 0);
    };
    tokio::select! {
        error = reporting => panic!("{error}"),
        () = checks => {}
    }
}

#[tokio::test]
async fn a_heartbeat_sent_while_a_slow_active_report_is_in_flight_follows_it() {
    let mut harness = Harness::new().await;
    harness.deploy("dep_a", harness.valid());
    *harness.management.progress_delay.lock().unwrap() = Duration::from_millis(200);
    let reporter = Reporter::new(harness.management_config().client(), CancellationToken::new());
    let observed = Observed { lease: 0, revision: 1, ..Observed::default() };
    let active = super::super::progress("dep_a", DeploymentState::Active, String::new());
    let (active, heartbeat) =
        tokio::join!(reporter.send(observed.clone(), Some(active)), reporter.send(observed, None));
    assert!(matches!((active, heartbeat), (Ok(true), Ok(true))));
    let applied: Vec<_> = std::iter::from_fn(|| harness.reported.try_recv().ok())
        .map(|report| (report.sequence, report.deployment.map(|progress| progress.state())))
        .collect();
    assert_eq!(applied, [(1, Some(DeploymentState::Active)), (2, None)]);
}

#[tokio::test]
async fn no_report_queued_behind_one_in_flight_is_sent_once_core_is_stopping() {
    let mut harness = Harness::new().await;
    harness.deploy("dep_a", harness.valid());
    *harness.management.progress_delay.lock().unwrap() = Duration::from_millis(200);
    let stopping = CancellationToken::new();
    let reporter = Reporter::new(harness.management_config().client(), stopping.clone());
    let observed = Observed { lease: 0, revision: 1, ..Observed::default() };
    let progress = |state| super::super::progress("dep_a", state, String::new());
    let sent = tokio::join!(
        reporter.send(observed.clone(), Some(progress(DeploymentState::InProgress))),
        reporter.send(observed.clone(), None),
        reporter.send(observed, Some(progress(DeploymentState::Active))),
        async { stopping.cancel() },
    );
    assert!(matches!(sent, (Ok(true), Ok(false), Ok(false), ())));
    let applied: Vec<_> =
        std::iter::from_fn(|| harness.reported.try_recv().ok()).map(|report| report.sequence).collect();
    assert_eq!(applied, [1]);
}

#[tokio::test]
async fn a_report_fenced_under_the_held_lease_stops_core_while_its_attach_delivers_nothing() {
    let mut harness = Harness::new().await;
    harness.management.publish(&mut harness.management.records.lock().unwrap());
    let core = Core::start(harness.core(), || {}).await.unwrap();
    let (gateway, lease) = (OnceLock::new(), watch::Sender::new(Lease::Waiting));
    let published = lease.subscribe();
    let managed =
        Managed::new(&harness.management_config(), lease, "env_test".into(), &harness.state(), &core, &gateway, None);
    let mut running = Box::pin(managed.run());
    tokio::select! {
        error = &mut running => panic!("{error}"),
        report = harness.reported.recv() => assert_eq!(report.unwrap().lease, 1),
    }

    tokio::time::pause();
    let time = hold_time();
    // Another core attaches, and this core's attach never hears of it.
    harness.management.attach_held.send_replace(true);
    harness.management.lease.send_replace(2);
    let error = tokio::select! {
        error = &mut running => error,
        () = advance_for(INTERVAL * 2) => panic!("core kept serving"),
    };
    assert!(error.to_string().contains("fenced"), "{error}");
    assert_eq!(*published.borrow(), Lease::Superseded);

    drop((running, time));
    tokio::time::resume();
    core.stop(|| {}).await.unwrap();
}

#[tokio::test]
async fn a_report_fenced_while_core_attaches_again_leaves_it_serving_under_the_new_lease() {
    let mut harness = Harness::new().await;
    harness.management.publish(&mut harness.management.records.lock().unwrap());
    let core = Core::start(harness.core(), || {}).await.unwrap();
    let (gateway, lease) = (OnceLock::new(), watch::Sender::new(Lease::Waiting));
    let published = lease.subscribe();
    let managed =
        Managed::new(&harness.management_config(), lease, "env_test".into(), &harness.state(), &core, &gateway, None);
    let mut running = Box::pin(managed.run());
    tokio::select! {
        error = &mut running => panic!("{error}"),
        report = harness.reported.recv() => assert_eq!(report.unwrap().lease, 1),
    }

    tokio::time::pause();
    let time = hold_time();
    // The attach stalls, so core attaches again, and management grants lease 2 but holds its answer back while a
    // report under lease 1 is fenced.
    harness.management.attach_held.send_replace(true);
    let fenced = async {
        harness.management.lease.subscribe().wait_for(|lease| *lease == 2).await.unwrap();
        harness.management.fencing.notified().await;
    };
    tokio::select! {
        error = &mut running => panic!("{error}"),
        () = fenced => {}
        () = advance_for(ATTACH_IDLE + REATTACH + INTERVAL * 2) => panic!("no report was fenced"),
    }
    assert_eq!(*published.borrow(), Lease::Held(1));

    harness.management.attach_held.send_replace(false);
    let report = loop {
        tokio::select! {
            error = &mut running => panic!("{error}"),
            report = harness.reported.recv() => {
                let report = report.unwrap();
                if report.lease != 1 {
                    break report;
                }
            }
        }
    };
    assert_eq!((report.lease, report.sequence), (2, 1));
    assert_eq!(*published.borrow(), Lease::Held(2));

    drop((running, time));
    tokio::time::resume();
    core.stop(|| {}).await.unwrap();
}
