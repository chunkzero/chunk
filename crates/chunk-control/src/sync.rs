//! What each JVM reported over its topic, which control commits as it arrives. A JVM's first report on each stream is
//! complete and replaces what earlier streams reported; later reports on the stream merge into it.

mod links;

use std::{collections::BTreeMap, time::Duration};

use chunk_proto::sync::v1::{JvmReport, JvmSession, JvmSessionPhase};

use crate::{Control, Error, JvmIdentity, Result, RuntimeConnection, state::State};
pub(crate) use links::Links;

impl Control {
    /// Within a commit, accepts `identity`'s JVM running `host` if `token` is its credential, applies its complete
    /// `report` and replaces the host's link with a new stream. Returns the stream's ID.
    pub(crate) fn attach_in(
        &self,
        state: &mut State,
        host: &str,
        token: &str,
        identity: &JvmIdentity,
        report: &JvmReport,
    ) -> Result<u64> {
        let runtime = self.registered(state, host, identity)?;
        if runtime.token != token {
            return Err(Error::Invalid("invalid process credential"));
        }
        apply(state, host, report)?;
        self.links.attach(host, runtime.identity, report)
    }

    /// Within a commit, applies a later `report` of `identity`'s JVM from `stream`, which must still be `host`'s link.
    pub(crate) fn merge_in(
        &self,
        state: &mut State,
        host: &str,
        stream: u64,
        identity: &JvmIdentity,
        report: &JvmReport,
    ) -> Result<()> {
        self.registered(state, host, identity)?;
        apply(state, host, report)?;
        self.links.merge(host, stream, identity, report)
    }

    /// The process currently registered for `host`, if `identity` names it.
    fn registered(&self, state: &State, host: &str, identity: &JvmIdentity) -> Result<RuntimeConnection> {
        let runtime = self
            .host
            .connection(host)
            .filter(|runtime| runtime.identity == *identity && !state.released(host))
            .ok_or(Error::Invalid("unregistered or replaced process"))?;
        if let Some(expected) = state.hosts.get(host)
            && !crate::placement::runs_host(state, &runtime, expected)
        {
            return Err(Error::Invalid("process runs another host"));
        }
        Ok(runtime)
    }

    /// Applies the latest phase `operation`'s JVM reported, which it may have reported before control recorded the
    /// claim's assignment.
    pub(crate) fn apply_reported(&self, operation: &str) -> Result<()> {
        self.update(|state| {
            let Some(host) = state
                .claims
                .get(operation)
                .and_then(|claim| state.sessions.get(&claim.session))
                .map(|s| s.host.clone())
            else {
                return Ok(());
            };
            let Some(runtime) = self.host.connection(&host) else {
                return Ok(());
            };
            match self.links.delivery(&host, &runtime.identity, operation) {
                Some(status) => crate::delivery::apply(state, &host, &status),
                None => Ok(()),
            }
        })
    }

    /// Reapplies everything `identity` reported on `host`'s current stream. Reading the link inside the commit keeps
    /// it from overwriting a newer report.
    pub(crate) fn reapply(&self, state: &mut State, host: &str, identity: &JvmIdentity) -> Result<()> {
        match self.links.report(host, identity) {
            Some(report) => apply(state, host, &report),
            None => Ok(()),
        }
    }

    /// Waits until `runtime` reports session `id` ready.
    /// # Errors
    /// Reports a session that failed, ended, or did not start within 10 seconds.
    pub(crate) async fn session_ready(&self, host: &str, runtime: &RuntimeConnection, id: &str) -> Result<()> {
        let mut reports = self.links.subscribe();
        let ready = async {
            loop {
                if let Some(observed) = self.links.session(host, &runtime.identity, id) {
                    let state = self.state()?;
                    let session = state.sessions.get(id).ok_or(Error::Invalid("missing session"))?;
                    if !crate::sessions::matches(id, session, &observed) {
                        return Err(Error::Invalid("session inventory binding mismatch"));
                    }
                    match observed.phase() {
                        JvmSessionPhase::Ready => return Ok(()),
                        JvmSessionPhase::Starting => {}
                        _ => return Err(Error::Unresolved("session is not ready")),
                    }
                }
                reports.changed().await.map_err(|_| Error::Unresolved("session is not ready"))?;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), ready)
            .await
            .unwrap_or(Err(Error::Unresolved("session is not ready")))
    }
}

/// Records one report. Only a commit that also checks the report's stream, or reads the link, may call this.
fn apply(state: &mut State, host: &str, report: &JvmReport) -> Result<()> {
    for status in &report.deliveries {
        crate::delivery::apply(state, host, status)?;
    }
    for observed in &report.sessions {
        crate::sessions::apply(state, host, observed);
    }
    Ok(())
}

/// The sessions `host`'s JVM should run or end, by ID.
pub(crate) fn desired(state: &State, host: &str) -> Result<BTreeMap<String, JvmSession>> {
    let mut desired = BTreeMap::new();
    for (id, session) in state.sessions.iter().filter(|(_, session)| session.host == host && !session.finished) {
        let session = JvmSession {
            session_type: session.session_type.clone(),
            capacity: session.capacity,
            configuration_json: serde_json::to_vec(&session.configuration)?,
            finish: session.finish_requested,
        };
        desired.insert(id.clone(), session);
    }
    Ok(desired)
}
