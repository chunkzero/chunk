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
    Quoted, SuggestionQuery, quoted, unquoted_word, valid_suggestion, visible_commands,
};
mod deployment;
mod destinations;
pub use destinations::{Destination, DestinationManifest, DestinationOverflow, DestinationPolicy, SessionCreation};
mod domains;
mod effects;
pub use domains::{DOMAIN_MANIFEST_VERSION, DomainManifest, DomainScope, Hook, HookEvent, domain_path};
pub use effects::{Effect, EffectDestination, EffectMethod, MoveRefusal};
mod index;
mod schema;
mod session_configurations;
mod session_methods;
pub use session_configurations::{
    MAX_SESSION_CONFIGURATION_BYTES, SessionConfigurationDeclaration, SessionConfigurations,
    validate_session_configuration,
};
pub use session_methods::{SessionMethodDeclaration, SessionMethods};

pub use connections::ControlConnection;

pub use deployment::{
    CONTRACT_VERSION, Contracts, Deployment, Function, FunctionKind, RuntimeProfile, Visibility, validate_wire_value,
};
pub use index::{IndexQuery, compare_index_values};
pub use schema::{DatabaseSchema, Field, Schema, TableSchema, validate, validate_name};

#[cfg(test)]
mod tests;
