#![allow(unsafe_code)]

use crate::{
    Cancellation, Error, Execution, Limits,
    deadline::Deadline,
    runtime::{Prepared, State},
};

/// A runtime is exited while parked. All access and destruction enter it on its
/// owning thread. State (and therefore this wrapper) is neither Send nor Sync.
pub(crate) struct Runtime(Option<State>);

impl Runtime {
    pub(crate) fn load(
        executor: &tokio::runtime::Runtime,
        deadline: &Deadline,
        source: &str,
        limits: Limits,
        cancellation: &Cancellation,
    ) -> Result<Self, Error> {
        let _executor = executor.enter();
        let mut state = State::new(limits);
        // SAFETY: JsRuntime creation enters this isolate. No handle scopes remain;
        // exiting balances creation and restores the previously current isolate.
        unsafe { state.runtime.v8_isolate().exit() };
        let mut runtime = Self(Some(state));
        runtime.with(|state| state.initialize_on(executor, deadline, source, limits, cancellation))?;
        Ok(runtime)
    }

    pub(crate) fn calls(&self) -> u32 {
        self.0.as_ref().expect("live runtime").calls
    }

    pub(crate) fn execute(
        &mut self,
        executor: &tokio::runtime::Runtime,
        deadline: &Deadline,
        prepared: Prepared,
        limits: Limits,
        cancellation: &Cancellation,
    ) -> Result<Execution, Error> {
        let _executor = executor.enter();
        self.with(|state| state.execute(executor, deadline, prepared, limits, cancellation))
    }

    fn with<T>(&mut self, run: impl FnOnce(&mut State) -> T) -> T {
        let state = self.0.as_mut().expect("live runtime");
        // SAFETY: The wrapper cannot cross threads, and exclusive access prevents
        // concurrent use. Entered restores the previous isolate even during unwind.
        unsafe { state.runtime.v8_isolate().enter() };
        let entered = Entered(state);
        run(entered.0)
    }
}

struct Entered<'a>(&'a mut State);

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        // SAFETY: with() entered this isolate on this thread; its private callback
        // has returned or unwound, dropping all scopes before this matching exit.
        unsafe { self.0.runtime.v8_isolate().exit() };
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Some(mut state) = self.0.take() {
            // SAFETY: This parked isolate remains owned by this thread. State drops
            // its persistent handles before JsRuntime, whose OwnedIsolate destructor
            // performs the matching exit and disposal while this isolate is current.
            unsafe { state.runtime.v8_isolate().enter() };
            drop(state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwind_exits_the_isolate_before_another_runtime_is_entered() {
        crate::Engine::init_platform();
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let deadline = Deadline::new().unwrap();
        let load = || {
            Runtime::load(
                &executor,
                &deadline,
                "export default () => 42;",
                Limits::default(),
                &Cancellation::default(),
            )
            .unwrap()
        };
        let mut first = load();
        let mut second = load();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| first.with(|_| panic!("unwind")))).is_err());
        second.with(|state| {
            assert!(state.runtime.execute_script("check", "42").is_ok());
        });
        drop(first);
        drop(second);
    }
}
