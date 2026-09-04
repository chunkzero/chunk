//! The application contract and manifest, as data.
//!
//! The contract IR is what the edge compiler produces from `edge/`: tables and
//! their validators, functions with their kind, visibility and argument shapes,
//! listeners, commands, queues, crons, and session names with creation
//! parameters. The manifest is what `chunk build` writes into `dist/` and what
//! the platform reads to run an app.
//!
//! Everything downstream consumes this crate instead of the TypeScript source:
//! runtime argument validation, schema installation, Kotlin, Java and
//! TypeScript code generation, wire serialization, and test fakes.
//!
//! This crate is pure data and serialization. It has no I/O, no JavaScript,
//! and no dependency on any other chunk crate.
