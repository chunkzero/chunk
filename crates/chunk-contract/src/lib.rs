//! Language-independent application schema declarations.
//!
//! Build tooling resolves declarations into this contract; storage adapters map
//! it to physical tables and indexes. This crate has no I/O or JavaScript execution.

mod app;
pub use app::{AppArtifact, SessionDeclaration, class_name};

mod commands;
mod connections;
pub use commands::{
    Command, CommandArgument, CommandParser, CommandRoute, CommandSuggestions, MAX_COMMAND_INPUT, ParsedCommand,
    SuggestionQuery, visible_commands,
};
mod deployment;
mod domains;
pub use domains::{DOMAIN_MANIFEST_VERSION, DomainManifest, DomainScope, Hook, HookEvent, domain_path};
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
