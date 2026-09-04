//! Rust-owned state for one app.
//!
//! One SQLite database per app holding its tables, the durable scheduler and
//! cron jobs, and queue membership. Mutations pass through a single writer so
//! they are serializable without application-level retries. Subscription
//! invalidation is tracked beside the data and reported on commit so the edge
//! can push updated query results to sessions.
//!
//! The store outlives and is independent of the JavaScript runtime: an edge
//! reload swaps the runtime while the database stays open. Table shapes come
//! from the contract; this crate installs and migrates them but never
//! interprets edge code.
