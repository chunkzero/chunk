use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::MachineProfile;

#[cfg(feature = "compiler")]
mod archive;
mod descriptor;
#[cfg(feature = "compiler")]
mod directory;
mod jars;
mod launcher;
mod manifest;
#[cfg(feature = "compiler")]
mod publish;
mod session_configurations;
mod session_methods;
mod unpack;
mod verify;
pub use descriptor::{JavaRuntime, JvmDescriptor, read_jvm_descriptor};
#[cfg(feature = "compiler")]
pub use publish::{Release, ReleaseInputs, publish_release};
pub use unpack::{ArchiveDigest, UnpackLimits, unpack_release};
pub use verify::{VerifiedRelease, verify_release};

#[derive(Deserialize, Serialize)]
struct Metadata {
    version: u32,
    java_version: u32,
    apps: Vec<chunk_contract::AppArtifact>,
    profiles: BTreeMap<String, MachineProfile>,
    assets: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct Manifest<'a> {
    id: &'a str,
    #[serde(flatten)]
    metadata: &'a Metadata,
}

fn content_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(all(test, feature = "compiler"))]
mod tests;
