//! Runs an app's compiled edge bundle.
//!
//! One `QuickJS` runtime and heap per loaded app, reused between calls and
//! replaced as a unit on deployment. Invocation is `invoke(function ref,
//! args, context, deadline)`: no HTTP, no request objects. The runtime has
//! no ambient filesystem, network, environment or process API; everything
//! useful arrives as host capabilities installed on `ctx`, and which
//! capabilities exist depends on the function kind (query, mutation, action,
//! listener).
//!
//! The same executor runs the capability-free compiler pass that evaluates
//! `edge/` declarations into the contract IR. The executor interface is
//! engine-independent; `QuickJS` via rquickjs is the implementation. Heap,
//! stack and deadline limits contain application mistakes and are not the
//! security boundary for untrusted code; that is the hosting process.
