//! The session runtime supervisor.
//!
//! Starts one JVM process per app on a host, hands it the manifest, an
//! address and a token, and terminates the process's single connection:
//! session commands down (create, end, call, prepare, stop), events and
//! health up, player frame streams in both directions, and `EdgeCall`
//! relayed to the edge. Restarts a process with its players held at the edge.
//!
//! A `Host` is the backend that provides the machine: a plain local process,
//! a container runtime, a cloud sandbox. Hosts know how to start, pause,
//! resume and stop; the supervisor knows what a session process is. Isolation
//! per app is by process, container or microVM depending on the host.
