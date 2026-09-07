//! Application declarations and immutable deployment manifests. Scaffold only.
//!
//! Intended data includes environment/deployment identity, schemas, versioned
//! functions, server JAR and asset references, app metadata, session contracts,
//! domain trees, command/hook descriptors and optional machine requirements.
//! Server code owns routing policies; app metadata does not require matchmaking.
//!
//! Downstream consumers will use this contract for validation, generated
//! clients and deployment orchestration. Its exact representation remains
//! open. This crate has no I/O or JavaScript execution.
