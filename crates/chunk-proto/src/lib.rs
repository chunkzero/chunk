//! Rust bindings for the internal transport in `proto/`.
//!
//! The `.proto` files are the source of truth and are shared with the JVM
//! module of the same name. This crate only generates and re-exports the
//! message and service types; it holds no behavior. Application developers
//! never see these types: the generated `Edge` client and session stubs are
//! the only surface.
