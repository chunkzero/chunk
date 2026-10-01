use std::{future::Future, pin::Pin, sync::Arc};

use chunk_js::{Cancellation, Json, Mode};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};

use crate::{
    Error, Result,
    service::{Event, Request, Update},
};

/// Allocate before submitting so a lost acceptance reply does not require a new
/// identity. IDs belong to one backend process; they cannot be replayed after restart.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ActionId {
    pub(crate) incarnation: String,
    pub(crate) sequence: u64,
}

impl std::fmt::Display for ActionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.incarnation, self.sequence)
    }
}

/// Parses the `<incarnation>:<sequence>` form [`ActionId`] displays as.
impl std::str::FromStr for ActionId {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        let invalid = || Error::Invalid("action identity");
        let (incarnation, sequence) = text.rsplit_once(':').ok_or_else(invalid)?;
        if incarnation.is_empty() || !sequence.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
        Ok(Self { incarnation: incarnation.to_owned(), sequence: sequence.parse().map_err(|_| invalid())? })
    }
}

/// What an identity from [`crate::Backend::allocate_action_id`] names, resolved without consulting any deployment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionIdentity {
    /// Prepared, and nothing started under it yet.
    Unused,
    /// An action running or retained under it.
    Action,
    /// A hook running or retained under it.
    Hook,
}

#[derive(Clone, Debug)]
pub enum ActionStatus {
    Running,
    /// Failure or cancellation does not undo mutations already committed by this action.
    Finished(Result<Arc<str>>),
}

pub(crate) struct Scope(pub Cancellation);
impl Drop for Scope {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// A caller-owned action scope. Dropping the last handle cancels further effects;
/// accepted mutations may still commit and retain their derived operation IDs.
#[derive(Clone)]
pub struct ActionHandle {
    pub(crate) id: ActionId,
    pub(crate) status: watch::Receiver<ActionStatus>,
    pub(crate) scope: Arc<Scope>,
}

impl ActionHandle {
    #[must_use]
    pub fn id(&self) -> &ActionId {
        &self.id
    }

    pub fn cancel(&self) {
        self.scope.0.cancel();
    }

    #[must_use]
    pub fn status(&self) -> ActionStatus {
        self.status.borrow().clone()
    }

    /// # Errors
    /// Reports application failure, cancelled scope, deadline, or an unknown outcome
    /// if the backend disappears. No automatic retries are performed.
    pub async fn outcome(&mut self) -> Result<Arc<str>> {
        loop {
            if let ActionStatus::Finished(result) = self.status.borrow().clone() {
                return result;
            }
            self.status.changed().await.map_err(|_| Error::ActionOutcomeUnknown)?;
        }
    }
}

pub(crate) struct Host {
    pub id: ActionId,
    pub events: mpsc::Sender<Event>,
    pub slots: Arc<Semaphore>,
    pub cancellation: Cancellation,
    pub effects: Arc<crate::effects::ScopedEffects>,
    /// Set for an action, whose platform effects are moves; a command's go to its runner, and a hook has none.
    pub moves: Option<crate::moves::Slot>,
}

struct CancelEffect(Cancellation);
impl Drop for CancelEffect {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl chunk_js::ActionHost for Host {
    fn platform(
        &self,
        sequence: u32,
        request: Json,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<String, String>>>> {
        if let Some(moves) = &self.moves {
            let result = if self.cancellation.is_cancelled() {
                Err(Error::Cancelled.to_string())
            } else {
                let operation = crate::commands::effect_operation(&self.effects.invocation, sequence);
                crate::moves::perform(moves, &operation, &request)
            };
            return Box::pin(async move { result });
        }
        let id = self.id.clone();
        let events = self.events.clone();
        let cancellation = self.cancellation.clone();
        let permit = self.slots.clone().try_acquire_owned();
        Box::pin(async move {
            let permit = permit.map_err(|_| Error::Busy.to_string())?;
            if cancellation.is_cancelled() {
                return Err(Error::Cancelled.to_string());
            }
            let effect_cancellation = Cancellation::default();
            let _cancel = CancelEffect(effect_cancellation.clone());
            let (reply, response) = oneshot::channel();
            let reply = Request::new(effect_cancellation, reply, permit);
            events
                .try_send(Event::ActionPlatform { id, sequence, request, reply })
                .map_err(|_| Error::Busy.to_string())?;
            let result: Arc<str> = response
                .await
                .map_err(|_| Error::ActionOutcomeUnknown.to_string())?
                .map_err(|error| error.to_string())?;
            Ok(result.to_string())
        })
    }

    fn http(
        &self,
        sequence: u32,
        request: chunk_js::HttpRequest,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<chunk_js::HttpOutcome, String>>>> {
        let effects = self.effects.clone();
        Box::pin(async move { Ok(effects.http(sequence, request).await) })
    }

    fn secret(&self, name: &str) -> std::result::Result<String, String> {
        if self.cancellation.is_cancelled() || std::time::Instant::now() >= self.effects.deadline {
            return Err("Secret capability expired".into());
        }
        self.effects.grants.secret(name)
    }

    fn call(
        &self,
        sequence: u32,
        mode: Mode,
        function: String,
        arguments: Json,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<String, String>>>> {
        let id = self.id.clone();
        let events = self.events.clone();
        let cancellation = self.cancellation.clone();
        let permit = self.slots.clone().try_acquire_owned();
        Box::pin(async move {
            let permit: OwnedSemaphorePermit = permit.map_err(|_| Error::Busy.to_string())?;
            if cancellation.is_cancelled() {
                return Err(Error::Cancelled.to_string());
            }
            let effect_cancellation = Cancellation::default();
            let _cancel = CancelEffect(effect_cancellation.clone());
            let (reply, response) = oneshot::channel();
            let request = Request::new(effect_cancellation, reply, permit);
            events
                .try_send(Event::ActionTransaction { id, sequence, mode, function, arguments, reply: request })
                .map_err(|error| {
                    match error {
                        mpsc::error::TrySendError::Full(_) => Error::Busy,
                        mpsc::error::TrySendError::Closed(_) => Error::ActionOutcomeUnknown,
                    }
                    .to_string()
                })?;
            let update: Update = response
                .await
                .map_err(|_| Error::ActionOutcomeUnknown.to_string())?
                .map_err(|error| error.to_string())?;
            Ok(update.json.to_string())
        })
    }
}
