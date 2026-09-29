//! Status reports against the fake management.

use super::*;
use crate::managed::status::{INTERVAL, OBSERVE, Observed, PrivateAddress, Reporter};
use std::net::{IpAddr, SocketAddr};

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
    // While a blocking task runs, paused time advances only through `tick`, however long a report takes.
    let (_release, held) = std::sync::mpsc::channel::<()>();
    tokio::task::spawn_blocking(move || held.recv());
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
