//! Control's system table rows. Each type serializes to exactly the document its table declares.

use chunk_proto::control::v1::{ClaimIdentity, ClaimPhase, ClaimRequest};
use prost::Message;
use serde::{Deserialize, Serialize};

use std::sync::Arc;

use crate::{Error, Release, Result};

/// The `(epoch, revision)` of a commit. Order and compare the pair; a restore starts a new epoch and may reuse revisions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Generation {
    pub epoch: u64,
    pub revision: u64,
}

const REVISION_BITS: u32 = 40;
const EPOCH_BITS: u32 = 23;

impl Generation {
    /// Stands for the commit applying the current update until the writer stamps in that commit's generation. No
    /// commit has it.
    pub(crate) const PENDING: Self = Self { epoch: u64::MAX, revision: u64::MAX };

    /// # Errors
    /// Rejects a pair that the wire form cannot carry.
    pub(crate) fn new(epoch: u64, revision: u64) -> Result<Self> {
        if epoch == 0 || epoch >> EPOCH_BITS != 0 || revision == 0 || revision >> REVISION_BITS != 0 {
            return Err(Error::Capacity);
        }
        Ok(Self { epoch, revision })
    }

    /// Reads [`Self::wire`]'s form, as JVMs report it.
    #[must_use]
    pub fn from_wire(wire: u64) -> Self {
        Self { epoch: wire >> REVISION_BITS, revision: wire & ((1 << REVISION_BITS) - 1) }
    }

    /// The `uint64` generation carried by the current wire contract: the epoch above 40 revision bits. It orders like
    /// the pair and stays a positive signed 64-bit value, as JVMs require.
    #[must_use]
    pub const fn wire(self) -> u64 {
        self.epoch << REVISION_BITS | self.revision
    }
}

/// A row that may name the commit writing it as [`Generation::PENDING`].
pub(crate) trait Stamp {
    /// Whether the row names [`Generation::PENDING`].
    fn pending(&self) -> bool {
        false
    }

    /// Replaces [`Generation::PENDING`] with `generation`.
    fn stamp(&mut self, _generation: Generation) {}
}

/// Replaces a wire generation naming [`Generation::PENDING`] with `generation`'s.
pub(super) fn stamp_wire(field: &mut u64, generation: Generation) {
    if *field == Generation::PENDING.wire() {
        *field = generation.wire();
    }
}

/// Control's singleton row.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Meta {
    /// Fingerprint of the environment configuration the state was written under.
    #[serde(with = "bytes")]
    pub config: Vec<u8>,
    pub method_sequence: u64,
    /// The release new placements use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
}

impl Stamp for Meta {
    fn pending(&self) -> bool {
        self.method_sequence == Generation::PENDING.wire()
    }

    fn stamp(&mut self, generation: Generation) {
        stamp_wire(&mut self.method_sequence, generation);
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Drain {
    #[serde(with = "bytes")]
    pub request: Vec<u8>,
    pub host: String,
    pub deadline_ms: u64,
    /// Started by control itself, so no caller retries it and it can be forgotten once the host stops.
    pub automatic: bool,
}

/// The method and arguments an operator's operation ID was first used for.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct OperatorCall {
    pub method: OperatorMethod,
    /// SHA-256 of the encoded arguments.
    #[serde(with = "bytes")]
    pub digest: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperatorMethod {
    MovePlayer,
    Drain,
}

impl OperatorMethod {
    pub const NAMES: [&str; 2] = ["move_player", "drain"];
}

/// A machine core minted a credential for, whose credential holds until it's revoked.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Machine {
    pub kind: MachineKind,
    pub created_at_ms: u64,
    pub revoked: bool,
}

/// What a machine credential authenticates as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineKind {
    Gateway,
    Jvm,
}

impl MachineKind {
    pub(crate) const NAMES: [&str; 2] = ["gateway", "jvm"];

    /// The kind's name in machine credentials and rows.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Jvm => "jvm",
        }
    }
}

/// What a remote runner starts on a host, and the runner boot bound to it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launch {
    /// The deployment whose release the host runs.
    pub deployment: String,
    /// The release whose archive the runner downloads.
    pub release: String,
    pub app: String,
    pub profile: String,
    pub process_id: String,
    pub generation: u64,
    /// The boot of the one runner that may start the host, once one asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot: Option<String>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MoveIntent {
    #[serde(with = "bytes")]
    pub request: Vec<u8>,
    pub canceled: bool,
    pub sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<MoveFailure>,
}

impl Stamp for MoveIntent {
    fn pending(&self) -> bool {
        self.sequence == Generation::PENDING.wire()
    }

    fn stamp(&mut self, generation: Generation) {
        stamp_wire(&mut self.sequence, generation);
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MoveFailure {
    pub reason: String,
    pub at_ms: u64,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HostState {
    /// The deployment version whose release this host runs.
    pub release: String,
    pub app: String,
    pub profile: String,
    pub retired: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_since_ms: Option<u64>,
    /// The capacity intent for this host, whose ID keys every host call made for it.
    #[serde(default)]
    pub capacity: Capacity,
    /// Why the host could not provide its capacity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

impl HostState {
    pub fn requested(release: &str, app: &str, profile: &str) -> Self {
        Self {
            release: release.into(),
            app: app.into(),
            profile: profile.into(),
            retired: false,
            idle_since_ms: None,
            capacity: Capacity::Requested,
            failure: None,
        }
    }
}

/// A release that hosts may run. A recorded release never changes, so rows compare by identity.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ReleaseState {
    #[serde(with = "json_release")]
    pub release: Arc<Release>,
    /// Retired releases place nothing new; their hosts are stopping.
    pub retired: bool,
    /// Set while the release drains, until it retires or becomes current again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain: Option<ReleaseDrain>,
}

impl PartialEq for ReleaseState {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.release, &other.release) && self.retired == other.retired && self.drain == other.drain
    }
}

/// When a release started draining, when it stops taking reconnects and when it stops regardless, in milliseconds
/// since the Unix epoch. Unset limits never pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReleaseDrain {
    pub since: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconnects_until: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stops_at: Option<u64>,
}

/// A host's capacity in lifecycle order. Control commits `Requested` and `Releasing`; the capacity executor commits
/// the rest once the host confirms them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Capacity {
    #[default]
    Requested,
    Ready,
    Releasing,
    Released,
}

impl Capacity {
    pub const NAMES: [&str; 4] = ["requested", "ready", "releasing", "released"];
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_since_ms: Option<u64>,
    pub finish_requested: bool,
    pub finished: bool,
    pub host: String,
    pub session_type: String,
    /// Empty for a session recovery found running without a log row.
    pub demand_key: String,
    pub capacity: u32,
    #[serde(with = "json_text")]
    pub configuration: serde_json::Value,
    pub retired: bool,
}

impl SessionState {
    /// Whether recovery recorded this session from its JVM's inventory, rather than placement creating it.
    pub fn recovered(&self) -> bool {
        self.demand_key.is_empty()
    }
}

/// Which claims a player owns. The row exists only while it names one.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlayerState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<String>,
}

/// Claim phases in lifecycle order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Reserved,
    Activating,
    Attached,
    Arrived,
    Withdrawing,
    Released,
}

impl Phase {
    pub const NAMES: [&str; 6] = ["reserved", "activating", "attached", "arrived", "withdrawing", "released"];
}

impl From<Phase> for ClaimPhase {
    fn from(phase: Phase) -> Self {
        match phase {
            Phase::Reserved => Self::Reserved,
            Phase::Activating => Self::Activating,
            Phase::Attached => Self::Attached,
            Phase::Arrived => Self::Arrived,
            Phase::Withdrawing => Self::Withdrawing,
            Phase::Released => Self::Released,
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Claim {
    #[serde(with = "bytes")]
    pub request: Vec<u8>,
    pub player: String,
    pub proxy: String,
    /// The commit that began this player's membership; a move keeps its source's.
    pub membership: Generation,
    /// The commit that created this claim.
    pub generation: Generation,
    pub session: String,
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "optional_bytes")]
    pub assignment: Option<Vec<u8>>,
    pub activated: bool,
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roster: Option<String>,
}

impl Claim {
    pub fn identity(&self, operation: &str) -> ClaimIdentity {
        ClaimIdentity {
            operation_id: operation.into(),
            proxy_id: self.proxy.clone(),
            membership_generation: self.membership.wire(),
            delivery_generation: self.generation.wire(),
        }
    }

    pub fn matches(&self, request: &ClaimRequest) -> Result<()> {
        if self.request.is_empty() {
            return Err(Error::Invalid("claim operation lost in a restore"));
        }
        if self.request != request.encode_to_vec() {
            return Err(Error::Invalid(crate::OPERATION_CHANGED));
        }
        Ok(())
    }
}

impl Stamp for Claim {
    fn pending(&self) -> bool {
        self.membership == Generation::PENDING || self.generation == Generation::PENDING
    }

    fn stamp(&mut self, generation: Generation) {
        for field in [&mut self.membership, &mut self.generation] {
            if *field == Generation::PENDING {
                *field = generation;
            }
        }
    }
}

impl Stamp for HostState {}
impl Stamp for SessionState {}
impl Stamp for PlayerState {}
impl Stamp for Drain {}
impl Stamp for OperatorCall {}
impl Stamp for Machine {}
impl Stamp for Launch {}
impl Stamp for Roster {}
impl Stamp for ReleaseState {}

/// Destination claims reserved together for a group move. Members are admitted together or not at all.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Roster {
    /// The caller's membership version.
    pub version: u64,
    /// Member claim operations, in request order.
    pub members: Vec<String>,
    /// Members whose activation is waiting for the rest.
    pub ready: Vec<String>,
    pub admitted: bool,
}

mod bytes {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(value))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        STANDARD.decode(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

mod optional_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    #[expect(clippy::ref_option, reason = "serde passes the field by reference")]
    pub fn serialize<S: Serializer>(value: &Option<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => super::bytes::serialize(value, serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error> {
        #[derive(Deserialize)]
        struct Wrapper(#[serde(with = "super::bytes")] Vec<u8>);
        Ok(Option::<Wrapper>::deserialize(deserializer)?.map(|Wrapper(bytes)| bytes))
    }
}

/// Arbitrary JSON kept as text, since a declared field cannot accept any value.
mod json_text {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &serde_json::Value, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<serde_json::Value, D::Error> {
        serde_json::from_str(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

mod json_release {
    use std::sync::Arc;

    use serde::{Deserialize, Deserializer, Serializer};

    use crate::Release;

    pub fn serialize<S: Serializer>(value: &Arc<Release>, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&serde_json::to_string(&**value).map_err(serde::ser::Error::custom)?)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Arc<Release>, D::Error> {
        Ok(Arc::new(serde_json::from_str(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)?))
    }
}
