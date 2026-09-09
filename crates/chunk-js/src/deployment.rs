use std::{sync::mpsc, thread::JoinHandle};

use crate::{Cancellation, Error, Execution, Invocation, Limits, ReadHost, model::bounds, runtime::Worker};

/// One immutable deployment in an environment, with a persistent runtime on its own
/// thread. The backend owns one handle per resident version; dropping it joins the
/// worker and releases the engine. This crate does not cache deployments globally.
pub struct Deployment {
    id: String,
    requests: Option<mpsc::SyncSender<Request>>,
    thread: Option<JoinHandle<()>>,
}

struct Request {
    invocation: Invocation,
    host: Box<dyn ReadHost>,
    cancellation: Cancellation,
    reply: mpsc::SyncSender<Result<Execution, Error>>,
}

impl Deployment {
    /// Starts the worker and loads the bundle once without invocation capabilities.
    /// Blocks until initialization completes under the supplied execution budget.
    /// # Errors
    /// Rejects invalid identity, source or limits, initialization failures and budgets.
    pub fn new(id: String, source: String, limits: Limits) -> Result<Self, Error> {
        if id.is_empty() || id.len() > bounds::NAME_BYTES {
            return Err(Error::Invalid("invalid deployment"));
        }
        deno_core::JsRuntime::init_platform(None);
        let (requests, incoming) = mpsc::sync_channel::<Request>(1);
        let (ready, initialized) = mpsc::sync_channel(1);
        let worker_id = id.clone();
        let thread = std::thread::Builder::new().name("chunk-js".into()).spawn(move || {
            let mut worker = match Worker::new(&worker_id, source, limits) {
                Ok(worker) => worker,
                Err(error) => {
                    let _ = ready.send(Err(error));
                    return;
                }
            };
            if ready.send(Ok(())).is_err() {
                return;
            }
            while let Ok(request) = incoming.recv() {
                let result = worker.execute(request.invocation, request.host, &request.cancellation);
                let _ = request.reply.send(result);
            }
        })?;
        let deployment = Self {
            id,
            requests: Some(requests),
            thread: Some(thread),
        };
        initialized.recv().map_err(|_| Error::WorkerStopped)??;
        Ok(deployment)
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Blocks while the worker executes this call. Exclusive access to the handle
    /// permits only one outstanding invocation; there is no unbounded request queue.
    /// Globals survive successful calls, but each call gets fresh host capabilities.
    /// # Errors
    /// Rejects invalid input, denied capabilities, unresolved promises, exceptions,
    /// cancellation, worker failure and heap/time limits. Errors return no writes.
    pub fn execute(
        &mut self,
        invocation: Invocation,
        host: Box<dyn ReadHost>,
        cancellation: &Cancellation,
    ) -> Result<Execution, Error> {
        let (reply, result) = mpsc::sync_channel(1);
        self.requests
            .as_ref()
            .ok_or(Error::WorkerStopped)?
            .send(Request {
                invocation,
                host,
                cancellation: cancellation.clone(),
                reply,
            })
            .map_err(|_| Error::WorkerStopped)?;
        result.recv().map_err(|_| Error::WorkerStopped)?
    }
}

impl Drop for Deployment {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
