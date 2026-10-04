//! A client for the management service's `chunk.management.v1` API. It speaks the Connect protocol over HTTP/1.1
//! with binary protobuf: unary calls and server streams, which is what the Bun-hosted service serves.
//!
//! It covers the calls its first users make, 44 of the API's 50 methods:
//!
//! - environments: `attach`, `report_status`, `set_wake_alarm`, `ensure_capacity`, `release_capacity`,
//!   `report_failed_auth`, `report_usage`, `report_logs` and `report_metrics`;
//! - edges: `watch_routes` and `wake`;
//! - the CLI: `start_login`, `poll_login`, `get_current_principal`, `revoke_token`, `create_project`,
//!   `list_projects`, `create_environment`, `get_environment`, `list_environments`, `delete_environment`,
//!   `upload_release`, `complete_release_upload`, `deploy`, `promote`, `rollback`, `get_deployment`,
//!   `list_deployments`, `list_apps`, `upload_assets`, `complete_asset_upload`, `get_asset_revision`,
//!   `list_asset_revisions`, `set_asset_head` and `read_logs`, plus `upload_archive` for release archives and
//!   asset blobs.
//!
//! Environments fetch the release archives `attach` names with `download_archive`, and asset blobs with
//! `download_blob`.
//!
//! The other methods, such as secrets, domains and the remaining token and environment calls, follow as callers need
//! them; each is one line in `methods.rs`.

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
