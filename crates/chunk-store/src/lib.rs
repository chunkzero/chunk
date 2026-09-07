//! Persistence abstraction for environment databases. Scaffold only.
//!
//! Planned adapters are Turso for hosted environments and SQLite, Postgres and
//! MySQL for self-hosting. Storage is independent of the embedded JS engine.
//!
//! The contract must support consistent reads, atomic durable commits and
//! recovery, including ambiguous commit outcomes. The split between storage
//! and the sync engine for snapshots, validation and change tracking remains
//! open. The sync engine owns transactional execution and reactive behavior.
//!
//! Old and new deployment code share an environment database under one
//! authoritative backend. Schema evolution uses additive changes and backfills
//! that preserve compatibility while old deployments remain referenced.
