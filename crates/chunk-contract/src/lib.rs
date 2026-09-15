//! Language-independent application schema declarations.
//!
//! Build tooling resolves declarations into this contract; storage adapters map
//! it to physical tables and indexes. This crate has no I/O or JavaScript execution.

mod app;
pub use app::{AppArtifact, SessionDeclaration, class_name};

mod connections;
mod deployment;
mod index;
mod schema;
mod session_methods;
pub use session_methods::{SessionMethodDeclaration, SessionMethods};

pub use connections::{BackendConnection, ControlConnection};

pub use deployment::{
    CONTRACT_VERSION, Deployment, Function, FunctionKind, RuntimeProfile, Visibility, validate_wire_value,
};
pub use index::{IndexQuery, compare_index_values};
pub use schema::{DatabaseSchema, Field, Schema, TableSchema, validate, validate_name};

#[cfg(test)]
mod tests;
