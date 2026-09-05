//! The control plane.
//!
//! Directory: which process hosts which session and which edge hosts an app's
//! database, looked up by ref. Placement: how many session processes an app
//! gets and which session types each hosts, one process for everything by
//! default. Provisioning: the bindings a manifest declares, such as pack
//! hosting and secrets. Also the natural issuer of transfer cookie keys.
//!
//! Serves `Directory` over the internal transport. Runs as `chunk control`
//! when self-hosting, or in-process for `chunk run` and `chunk dev`.
//!
//! Owns provider-neutral reconciliation and local/container host provisioning.
//! Runtimes report inventories and receive assignments through a public protocol.
//! Fly provisioning and commercial policy belong in a separate private platform.
//! These control services and adapters are not implemented yet.
