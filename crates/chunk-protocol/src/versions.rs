//! Release modules generated from pinned datasets owned by this crate.

#[cfg(feature = "mc-26-1")]
chunk_protocol_codegen::protocol_version!(v26_1, "data/26.1");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub name: &'static str,
    pub protocol: i32,
}

/// Enabled releases in oldest-to-newest order. The newest is advertised in
/// status replies; login is restricted to protocol IDs represented here.
pub const SUPPORTED: &[Version] = &[
    #[cfg(feature = "mc-26-1")]
    v26_1::VERSION,
];
