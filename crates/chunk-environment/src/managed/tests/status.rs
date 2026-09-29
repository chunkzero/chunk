//! Status reports against the fake management.

use super::*;
use crate::managed::status::{INTERVAL, OBSERVE, Observed, Reporter};
use std::net::SocketAddr;
use tokio::time::Instant;

async fn next(reported: &mut mpsc::UnboundedReceiver<ReportStatusRequest>) -> ReportStatusRequest {
    tokio::time::timeout(INTERVAL * 2, reported.recv()).await.expect("a report arrived").unwrap()
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

#[tokio::test(start_paused = true)]
async fn reports_repeat_under_one_lease_a_changed_address_reports_at_once_and_a_new_lease_starts_over() {
    let mut harness = Harness::new().await;
    // Paused time jumps to the next timer whenever the runtime waits, even on a report in flight, so a 1 ms timer keeps
    // each jump short.
    tokio::spawn(async {
        loop {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });
    let reporter = Reporter::new(harness.management_config().client());
    let observed = Mutex::new(Some(Observed { lease: 0, revision: 1, ..Observed::default() }));
    let reporting = reporter.keep_reporting(|| std::future::ready(observed.lock().unwrap().clone()));
    let checks = async {
        let since = Instant::now();
        let first = next(&mut harness.reported).await;
        assert_eq!((first.lease, first.sequence, first.desired_revision), (0, 1, 1));
        assert!(first.gateway_addresses.is_empty() && first.deployment.is_none() && !first.ready_to_suspend);
        for sequence in 2..=3 {
            let report = next(&mut harness.reported).await;
            assert_eq!((report.lease, report.sequence), (0, sequence));
        }
        assert!(since.elapsed() >= INTERVAL * 2 && since.elapsed() < (INTERVAL + OBSERVE) * 2, "{:?}", since.elapsed());

        let changed = Instant::now();
        observed.lock().unwrap().as_mut().unwrap().gateway_addresses = vec!["10.0.0.2:25565".into()];
        let report = next(&mut harness.reported).await;
        assert_eq!(report.sequence, 4);
        assert_eq!(report.gateway_addresses, ["10.0.0.2:25565"]);
        assert!(changed.elapsed() < OBSERVE * 2, "{:?}", changed.elapsed());

        // Under the next attach's lease, the sequence starts over.
        harness.management.lease.send_replace(1);
        observed.lock().unwrap().as_mut().unwrap().lease = 1;
        let report = next(&mut harness.reported).await;
        assert_eq!((report.lease, report.sequence), (1, 1));

        // Superseded or stopping, core reports nothing more.
        *observed.lock().unwrap() = None;
        assert!(tokio::time::timeout(INTERVAL * 3, harness.reported.recv()).await.is_err());
    };
    tokio::select! {
        never = reporting => match never {},
        () = checks => {}
    }
}
