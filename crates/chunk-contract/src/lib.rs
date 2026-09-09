//! Language-independent application schema declarations.
//!
//! Build tooling resolves declarations into this contract; storage adapters map
//! it to physical tables and indexes. This crate has no I/O or JavaScript execution.

mod deployment;
mod index;
mod schema;

pub use deployment::{
    CONTRACT_VERSION, Deployment, Function, FunctionKind, RuntimeProfile, Visibility, validate_wire_value,
};
pub use index::{IndexQuery, compare_index_values};
pub use schema::{DatabaseSchema, Field, Schema, TableSchema, validate, validate_name};

#[cfg(test)]
mod tests;
