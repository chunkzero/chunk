//! The session runtime supervisor.
//!
//! Starts one JVM process per app on a host, hands it the manifest, an
//! address and a token, and terminates the process's single connection:
//! session commands down (create, end, call, prepare, stop), events and
//! health up, player frame streams in both directions, and `EdgeCall`
//! relayed to the edge. Restarts a process with its players held at the edge.
//!
//! Supervises the JVM on its own host and reports inventory, readiness, and
//! capacity to a reconciler. Remote host provisioning belongs to chunk-control
//! or an external platform; this supervisor does not provision other machines.
//! The reconciliation client and lifecycle services are not implemented yet.
