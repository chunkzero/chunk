//! Common control-plane logic. Scaffold only.
//!
//! Owns the environment directory, automatic session placement, host
//! provisioning and deployment rollouts. Applications declare session types,
//! profiles and policies; chunk creates sessions and selects capacity.
//!
//! Gameplay JVMs can host multiple sessions of one environment, deployment
//! and machine profile. Host adapters target Fly and self-hosted containers.
//! Prewarmed capacity may suspend after loading. Machine age begins draining,
//! with graceful completion and a configurable shutdown deadline.
//!
//! Admission, failure and reconnect policies remain open. Proxy updates and
//! application rollouts have distinct drain lifecycles.
