//! Language-independent application schema declarations.
//!
//! Build tooling resolves declarations into this contract; storage adapters map
//! it to physical tables and indexes. This crate has no I/O or JavaScript execution.

mod schema;

pub use schema::{DatabaseSchema, Field, Schema, TableSchema, validate, validate_name};

#[cfg(test)]
mod tests;
