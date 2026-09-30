//! A client for the management service's `chunk.management.v1` API. It speaks the Connect protocol over HTTP/1.1
//! with binary protobuf: unary calls and server streams, which is what the Bun-hosted service serves.
//!
//! It covers the calls its first users make, 25 of the API's 44 methods:
//!
//! - environments: `attach`, `report_status`, `set_wake_alarm`, `ensure_capacity`, `release_capacity` and
//!   `report_failed_auth`;
//! - edges: `watch_routes` and `wake`;
//! - the CLI: `start_login`, `poll_login`, `get_current_principal`, `revoke_token`, `create_project`,
//!   `list_projects`, `create_environment`, `list_environments`, `upload_release`, `complete_release_upload`,
//!   `deploy`, `promote`, `rollback`, `get_deployment`, `list_deployments`, `list_apps` and `read_logs`, plus
//!   `upload_archive` for release bytes.
//!
//! Environments fetch the release archives `attach` names with `download_archive`.
//!
//! The other methods, such as secrets, domains, the remaining token and environment calls and the remaining reports,
//! follow as callers need them; each is one line in `methods.rs`.

mod client;
mod error;
mod methods;
mod stream;

pub use client::{Client, Download};
pub use error::{Code, Error, Status};
pub use stream::Stream;

/// The `chunk.management.v1` messages.
#[allow(clippy::all, clippy::pedantic)]
pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/chunk.management.v1.rs"));
}
