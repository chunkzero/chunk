//! One stream per JVM. Control sends the sessions the JVM should run or end, and the JVM writes back its actual
//! sessions and deliveries, which control commits as they arrive. A JVM's first report on each stream is complete and
//! gets one repair pass.

mod links;

use std::{collections::BTreeMap, time::Duration};

use chunk_proto::v1::{DesiredSessions, ProcessIdentity, ProcessReport, SessionCommand, SessionPhase, SessionRef};
use tokio::sync::mpsc;
use tokio_stream::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;
use tonic::Status;

use crate::{Control, Error, Result, RuntimeConnection, state::State};
pub(crate) use links::Links;

/// The command for each session control wants a JVM to run (`false`) or end (`true`).
type Desired = BTreeMap<String, (bool, SessionCommand)>;

impl Control {
    /// Serves one JVM's stream until either side or `closed` closes it. Each desired update waits for the reader, so a
    /// slow JVM receives only each session's latest command.
    pub(crate) async fn sync(
        &self,
        token: String,
        mut reports: impl Stream<Item = std::result::Result<ProcessReport, Status>> + Unpin,
        sender: mpsc::Sender<std::result::Result<DesiredSessions, Status>>,
        closed: CancellationToken,
    ) {
        let first = tokio::select! { () = closed.cancelled() => return, report = reports.next() => report };
        let Some(Ok(first)) = first else {
            return;
        };
        let host = first.identity.as_ref().map(|identity| identity.runtime_id.clone()).unwrap_or_default();
        let stream = match self.attach(&host, &token, first).await {
            Ok(stream) => stream,
            Err(error) => {
                let _ = sender.send(Err(crate::rpc::status(error))).await;
                return;
            }
        };
        let result = async {
            let mut positions = self.subscribe();
            let mut sent = None;
            loop {
                if let Some(update) = self.desired(&host, &mut sent)? {
                    tokio::select! {
                        () = closed.cancelled() => return Ok(()),
                        sent = sender.send(Ok(update)) => if sent.is_err() { return Ok(()) },
                    }
                }
                tokio::select! {
                    () = closed.cancelled() => return Ok(()),
                    changed = positions.changed() => if changed.is_err() { return Ok(()) },
                    report = reports.next() => match report {
                        Some(Ok(report)) => self.report(&host, stream, &report).await?,
                        _ => return Ok(()),
                    },
                }
            }
        }
        .await;
        self.links.detach(&host, stream);
        if let Err(error) = result {
            let _ = sender.send(Err(crate::rpc::status(error))).await;
        }
    }

    /// Accepts the JVM running `host` if `token` is its credential, then reconciles its complete `report` against the
    /// log once. Returns the stream's ID for later reports.
    /// # Errors
    /// Rejects an unregistered, replaced or stopped process.
    pub(crate) async fn attach(&self, host: &str, token: &str, report: ProcessReport) -> Result<u64> {
        let runtime = self.registered(host, report.identity.as_ref())?;
        if runtime.token != token {
            return Err(Error::Invalid("invalid process credential"));
        }
        let stream = self.links.attach(host, runtime.identity.clone(), &report)?;
        let applied = self.apply(host, &runtime.identity, &report);
        let fenced = self.fence_deliveries(&runtime, &report).await.map(|_| ());
        self.links.applied();
        applied.and(fenced)?;
        self.resolve_recovery().await?;
        Ok(stream)
    }

    /// Commits the changes a JVM reported on `stream`.
    /// # Errors
    /// Rejects reports from a replaced stream or process; stale deliveries within a report are ignored.
    pub(crate) async fn report(&self, host: &str, stream: u64, report: &ProcessReport) -> Result<()> {
        let runtime = self.registered(host, report.identity.as_ref())?;
        self.links.merge(host, stream, report)?;
        let applied = self.apply(host, &runtime.identity, report);
        self.links.applied();
        applied?;
        if !self.recovery.open()? {
            self.resolve_recovery().await?;
        }
        Ok(())
    }

    /// The process currently registered for `host`, if `identity` names it.
    fn registered(&self, host: &str, identity: Option<&ProcessIdentity>) -> Result<RuntimeConnection> {
        let runtime = self
            .host
            .connection(host)
            .filter(|runtime| Some(&runtime.identity) == identity && !self.host.stopped(host))
            .ok_or(Error::Invalid("unregistered or replaced process"))?;
        if let Some(expected) = self.state()?.hosts.get(host)
            && !self.runs_host(&runtime, expected)
        {
            return Err(Error::Invalid("process runs another host"));
        }
        Ok(runtime)
    }

    fn apply(&self, host: &str, identity: &ProcessIdentity, report: &ProcessReport) -> Result<()> {
        self.update(|state| {
            for binding in &report.deliveries {
                crate::delivery::apply(state, host, identity, binding)?;
            }
            for observed in &report.sessions {
                crate::sessions::apply(state, host, observed);
            }
            Ok(())
        })
    }

    /// Applies the latest phase `operation`'s JVM reported, which it may have reported before control recorded the
    /// claim's assignment.
    pub(crate) fn apply_reported(&self, operation: &str) -> Result<()> {
        let state = self.state()?;
        let Some(host) =
            state.claims.get(operation).and_then(|claim| state.sessions.get(&claim.session)).map(|s| &s.host)
        else {
            return Ok(());
        };
        let Some(runtime) = self.host.connection(host) else {
            return Ok(());
        };
        let Some(binding) = self.links.delivery(host, &runtime.identity, operation) else {
            return Ok(());
        };
        self.update(|state| crate::delivery::apply(state, host, &runtime.identity, &binding))
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
                    match SessionPhase::try_from(observed.phase) {
                        Ok(SessionPhase::Ready) => return Ok(()),
                        Ok(SessionPhase::Starting) => {}
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

    /// The sessions `host`'s JVM should run or end that differ from `sent`, or all of them when nothing was sent.
    /// `None` when nothing changed.
    pub(crate) fn desired(&self, host: &str, sent: &mut Option<Desired>) -> Result<Option<DesiredSessions>> {
        let state = self.state()?;
        let Some(runtime) = self.host.connection(host) else {
            return Err(Error::Invalid("unregistered or replaced process"));
        };
        let desired = desired(&state, host, &runtime.identity)?;
        let mut update = DesiredSessions::default();
        for (id, (finish, command)) in &desired {
            if sent.as_ref().and_then(|sent| sent.get(id)).is_some_and(|(was, sent)| was == finish && sent == command) {
                continue;
            }
            if *finish { &mut update.finish } else { &mut update.create }.push(command.clone());
        }
        let first = sent.is_none();
        *sent = Some(desired);
        Ok((first || !update.create.is_empty() || !update.finish.is_empty()).then_some(update))
    }
}

fn desired(state: &State, host: &str, identity: &ProcessIdentity) -> Result<Desired> {
    let mut desired = BTreeMap::new();
    for (id, session) in state.sessions.iter().filter(|(_, session)| session.host == host && !session.finished) {
        let command = SessionCommand {
            identity: Some(identity.clone()),
            operation_id: format!("session/{id}"),
            session: Some(SessionRef { id: id.clone() }),
            generation: 1,
            session_type: session.session_type.clone(),
            capacity: session.capacity,
            configuration_json: serde_json::to_vec(&session.configuration)?,
        };
        desired.insert(id.clone(), (session.finish_requested, command));
    }
    Ok(desired)
}
