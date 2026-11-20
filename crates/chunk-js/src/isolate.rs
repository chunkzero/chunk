#![allow(unsafe_code)]

use deno_core::v8;

use crate::{
    Cancellation, Error, Execution, Limits,
    deadline::Deadline,
    runtime::{Prepared, State},
};

struct MainContext(v8::Global<v8::Context>);

/// Lets [`interrupt`] enter the runtime's context.
pub(crate) fn prepare_interrupts(runtime: &mut deno_core::JsRuntime) {
    let context = runtime.main_context();
    runtime.v8_isolate().set_slot(MainContext(context));
}

/// Ends the isolate's JavaScript from any thread. Dispatching `undefined` before terminating
/// keeps `deno_core`, which converts the dispatched exception, from building its own error,
/// whose prototype application code controls.
pub(crate) fn interrupt(handle: &v8::IsolateHandle) {
    handle.request_interrupt(dispatch, std::ptr::null_mut());
}

extern "C" fn dispatch(mut isolate: v8::UnsafeRawIsolatePtr, _: *mut std::ffi::c_void) {
    // SAFETY: V8 runs interrupts on the isolate's thread while it executes JavaScript,
    // like a function callback.
    let isolate = unsafe { v8::Isolate::ref_from_raw_isolate_ptr_mut(&mut isolate) };
    let Some(MainContext(context)) = isolate.get_slot::<MainContext>() else { return };
    let context = context.clone();
    v8::callback_scope!(unsafe scope, isolate);
    v8::scope!(let scope, scope);
    let context = v8::Local::new(scope, context);
    let scope = &mut v8::ContextScope::new(scope, context);
    let undefined = v8::undefined(scope);
    deno_core::error::dispatch_exception(scope, undefined.into(), false);
}

/// A runtime is exited while parked. All access and destruction enter it on its
/// owning thread. State (and therefore this wrapper) is neither Send nor Sync.
pub(crate) struct Runtime(Option<State>);

impl Runtime {
    pub(crate) fn load(
        executor: &tokio::runtime::Runtime,
        deadline: &Deadline,
        specifier: &str,
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
        runtime.with(|state| state.initialize_on(executor, deadline, specifier, source, limits, cancellation))?;
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
mod tests;
