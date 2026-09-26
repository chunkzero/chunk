use std::{collections::BTreeMap, sync::Mutex};

use chunk_proto::v1::{DeliveryInventory, ProcessIdentity, ProcessReport, SessionInventory};
use tokio::sync::watch;

use crate::{Error, Result};

/// The actual state each attached JVM last reported, kept in memory: a JVM reports it again whenever it reconnects.
/// Attaching and merging happen inside the commit that applies the report, so links change in commit order.
#[derive(Default)]
pub(crate) struct Links {
    hosts: Mutex<BTreeMap<String, Link>>,
    streams: std::sync::atomic::AtomicU64,
    /// Counts applied reports, so waiters can watch for the next one.
    reports: watch::Sender<u64>,
}

struct Link {
    identity: ProcessIdentity,
    stream: u64,
    sessions: BTreeMap<String, SessionInventory>,
    deliveries: BTreeMap<String, DeliveryInventory>,
}

impl Link {
    fn merge(&mut self, report: &ProcessReport) {
        for session in &report.sessions {
            if let Some(reference) = &session.session {
                self.sessions.insert(reference.id.clone(), session.clone());
            }
        }
        for binding in &report.deliveries {
            if let Some(delivery) = &binding.delivery {
                self.deliveries.insert(delivery.operation_id.clone(), binding.clone());
            }
        }
    }
}

impl Links {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, Link>>> {
        self.hosts.lock().map_err(|_| Error::Unresolved("process links poisoned"))
    }

    /// Replaces `host`'s link with a stream whose first report is `report`, returning the stream's ID.
    pub fn attach(&self, host: &str, identity: ProcessIdentity, report: &ProcessReport) -> Result<u64> {
        let stream = self.streams.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let mut link = Link { identity, stream, sessions: BTreeMap::new(), deliveries: BTreeMap::new() };
        link.merge(report);
        self.lock()?.insert(host.into(), link);
        Ok(stream)
    }

    /// Merges a later report from `stream`. Rejects a replaced stream or another process.
    pub fn merge(&self, host: &str, stream: u64, report: &ProcessReport) -> Result<()> {
        let mut links = self.lock()?;
        let link = links
            .get_mut(host)
            .filter(|link| link.stream == stream && report.identity.as_ref() == Some(&link.identity))
            .ok_or(Error::Invalid("stale process report"))?;
        link.merge(report);
        Ok(())
    }

    pub fn detach(&self, host: &str, stream: u64) {
        if let Ok(mut links) = self.lock()
            && links.get(host).is_some_and(|link| link.stream == stream)
        {
            links.remove(host);
        }
    }

    /// Drops `host`'s reported sessions that control no longer tracks.
    pub fn forget(&self, host: &str, sessions: &[String]) {
        if let Ok(mut links) = self.lock()
            && let Some(link) = links.get_mut(host)
        {
            for id in sessions {
                link.sessions.remove(id);
            }
        }
    }

    /// Wakes waiters after a report was applied.
    pub fn applied(&self) {
        self.reports.send_modify(|count| *count += 1);
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.reports.subscribe()
    }

    /// Everything `identity` reported on `host`'s current stream.
    pub fn report(&self, host: &str, identity: &ProcessIdentity) -> Option<ProcessReport> {
        let links = self.hosts.lock().ok()?;
        let link = links.get(host).filter(|link| link.identity == *identity)?;
        Some(ProcessReport {
            identity: Some(link.identity.clone()),
            sessions: link.sessions.values().cloned().collect(),
            deliveries: link.deliveries.values().cloned().collect(),
        })
    }

    pub fn session(&self, host: &str, identity: &ProcessIdentity, id: &str) -> Option<SessionInventory> {
        let links = self.hosts.lock().ok()?;
        links.get(host).filter(|link| link.identity == *identity)?.sessions.get(id).cloned()
    }

    pub fn delivery(&self, host: &str, identity: &ProcessIdentity, operation: &str) -> Option<DeliveryInventory> {
        let links = self.hosts.lock().ok()?;
        links.get(host).filter(|link| link.identity == *identity)?.deliveries.get(operation).cloned()
    }
}
