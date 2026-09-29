//! Reporting the clients that failed authentication at the gateway, which management then stops from waking the
//! environment for a while.

use super::{Managed, REQUEST_TIMEOUT};
use chunk_management::v1;
use std::{convert::Infallible, time::Duration};

/// How often failures are reported, at most.
const INTERVAL: Duration = Duration::from_secs(5);

impl Managed<'_> {
    /// Reports the gateway's new failures every [`INTERVAL`]. Reporting is best effort: a batch management doesn't take
    /// is dropped.
    pub(super) async fn report_failed_auth(&self) -> Infallible {
        let mut tick = tokio::time::interval(INTERVAL);
        loop {
            tick.tick().await;
            let Some(gateway) = self.gateway.get() else { continue };
            let failures: Vec<_> = gateway
                .reports()
                .take_failed_auth()
                .into_iter()
                .map(|(client, time)| v1::FailedAuth { client_address: client.to_string(), time: Some(time.into()) })
                .collect();
            if failures.is_empty() {
                continue;
            }
            let count = failures.len();
            let request = v1::ReportFailedAuthRequest { failures };
            match tokio::time::timeout(REQUEST_TIMEOUT, self.client.report_failed_auth(&request)).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => tracing::warn!(%error, count, "failed authentications not reported"),
                Err(_) => tracing::warn!(count, "failed authentications not reported in time"),
            }
        }
    }
}
