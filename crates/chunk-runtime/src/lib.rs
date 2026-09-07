//! Local gameplay JVM supervision. Scaffold only.
//!
//! A JVM belongs to one environment, deployment and machine profile and can
//! host multiple independently scoped sessions. The control plane provisions
//! hosts and places sessions; the runtime supervises the local JVM and reports
//! health, session lifecycle and drain progress.
//!
//! Control, function calls and player transport need not share a connection.
//! Restarting a JVM does not restore its in-memory worlds or session state.
