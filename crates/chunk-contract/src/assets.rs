//! Asset revisions, the worlds, resource packs and files a deployment's apps read, and the asset declarations a
//! release makes.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const ASSET_REVISION_VERSION: u32 = 1;
/// The most worlds, packs and files one revision holds, together.
pub const MAX_ASSET_ENTRIES: usize = 4096;
pub const MAX_WORLD_BYTES: u64 = 256 * 1024 * 1024;
/// The largest pack a Minecraft client downloads.
pub const MAX_PACK_BYTES: u64 = 250 * 1024 * 1024;
pub const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;
/// The most distinct bytes one revision holds.
pub const MAX_REVISION_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 512;
const MAX_PROMPT_BYTES: usize = 1024;

/// One immutable asset revision: every world, pack and file a deployment's apps read, by content digest. Its ID is the
/// SHA-256 of [`encode`](Self::encode)'s canonical JSON.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetRevision {
    pub version: u32,
    /// Resource packs by name, each a zip a client downloads.
    pub packs: BTreeMap<String, PackBlob>,
    /// Files of the project's `assets/`, by path relative to it.
    pub shared: BTreeMap<String, AssetBlob>,
    /// Each app's worlds and files, by app ID.
    pub apps: BTreeMap<String, AppAssets>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppAssets {
    /// Polar worlds by name.
    pub worlds: BTreeMap<String, AssetBlob>,
    /// Files of the app's `assets/`, by path relative to it.
    pub files: BTreeMap<String, AssetBlob>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetBlob {
    /// Lowercase hex SHA-256 of the bytes.
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackBlob {
    pub sha256: String,
    /// Lowercase hex SHA-1, which clients check a downloaded pack against.
    pub sha1: String,
    pub size: u64,
}

impl AssetRevision {
    /// The canonical JSON the revision ID digests.
    /// # Panics
    /// Never: a revision holds only maps with string keys, which always serialize.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("asset revisions serialize")
    }

    #[must_use]
    pub fn id(&self) -> String {
        hex(&Sha256::digest(self.encode()))
    }

    /// Parses and validates a revision from exactly its canonical JSON.
    /// # Errors
    /// Rejects malformed or invalid revisions and noncanonical JSON, whose digest would not be the revision's ID.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let revision: Self =
            serde_json::from_slice(bytes).map_err(|error| format!("invalid asset revision: {error}"))?;
        revision.validate()?;
        if revision.encode() != bytes {
            return Err("asset revision JSON is not canonical".into());
        }
        Ok(revision)
    }

    /// # Errors
    /// Rejects unsupported versions, invalid names, paths and digests, and revisions over their limits.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != ASSET_REVISION_VERSION {
            return Err("unsupported asset revision version".into());
        }
        let mut entries = self.packs.len() + self.shared.len();
        for (name, pack) in &self.packs {
            if !name_valid(name) || !digest(&pack.sha256, 64) || !digest(&pack.sha1, 40) || pack.size > MAX_PACK_BYTES {
                return Err(format!("invalid pack {name}"));
            }
        }
        for (path, file) in &self.shared {
            check_file(path, file)?;
        }
        for (app, assets) in &self.apps {
            if !name_valid(app) {
                return Err(format!("invalid app ID {app}"));
            }
            entries += assets.worlds.len() + assets.files.len();
            for (name, world) in &assets.worlds {
                if !name_valid(name) || !digest(&world.sha256, 64) || world.size > MAX_WORLD_BYTES {
                    return Err(format!("invalid world {app}/{name}"));
                }
            }
            for (path, file) in &assets.files {
                check_file(path, file)?;
            }
        }
        if entries > MAX_ASSET_ENTRIES {
            return Err(format!("an asset revision holds at most {MAX_ASSET_ENTRIES} entries"));
        }
        let mut sizes = BTreeMap::new();
        for (sha256, size) in self.blob_entries() {
            if sizes.insert(sha256, size).is_some_and(|other| other != size) {
                return Err(format!("blob {sha256} is declared with different sizes"));
            }
        }
        if self.blobs().values().sum::<u64>() > MAX_REVISION_BYTES {
            return Err("asset revision size limit".into());
        }
        Ok(())
    }

    /// Every distinct blob the revision holds: its SHA-256 and size.
    #[must_use]
    pub fn blobs(&self) -> BTreeMap<&str, u64> {
        self.blob_entries().collect()
    }

    /// Every blob reference of the revision, repeats included.
    fn blob_entries(&self) -> impl Iterator<Item = (&str, u64)> {
        let packs = self.packs.values().map(|pack| (pack.sha256.as_str(), pack.size));
        let shared = self.shared.values().map(|blob| (blob.sha256.as_str(), blob.size));
        let apps = self.apps.values().flat_map(|assets| assets.worlds.values().chain(assets.files.values()));
        packs.chain(shared).chain(apps.map(|blob| (blob.sha256.as_str(), blob.size)))
    }

    /// The blobs a JVM of `app` reads: its worlds and files and the shared files, but no packs.
    #[must_use]
    pub fn app_blobs(&self, app: &str) -> BTreeMap<&str, u64> {
        let assets = self.apps.get(app);
        let own = assets.into_iter().flat_map(|assets| assets.worlds.values().chain(assets.files.values()));
        own.chain(self.shared.values()).map(|blob| (blob.sha256.as_str(), blob.size)).collect()
    }
}

fn check_file(path: &str, file: &AssetBlob) -> Result<(), String> {
    if !path_valid(path) || !digest(&file.sha256, 64) || file.size > MAX_FILE_BYTES {
        return Err(format!("invalid asset file {path}"));
    }
    Ok(())
}

/// The worlds and packs a release's apps need from the asset revision deployed with it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetContract {
    /// Each app's declared worlds.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub worlds: BTreeMap<String, BTreeSet<String>>,
    /// Every declared pack, by name, which is unique in the project.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub packs: BTreeMap<String, PackDeclaration>,
    /// The packs each app's players hold, in the order the client stacks them: its outermost scope's first, its own
    /// last. An app without packs has no entry.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub app_packs: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackDeclaration {
    /// Players who decline or fail to load the pack are disconnected.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    /// Plain text the client shows when it asks the player to accept the pack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// A pack resolved for a player: what the gateway sends in the client's `add_resource_pack`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPack {
    pub name: String,
    pub id: [u8; 16],
    pub url: String,
    pub sha1: String,
    pub required: bool,
    pub prompt: Option<String>,
}

impl AssetContract {
    /// # Errors
    /// Rejects invalid names and prompts, and app packs that name undeclared packs.
    pub fn validate(&self) -> Result<(), String> {
        for (app, worlds) in &self.worlds {
            if !name_valid(app) || worlds.iter().any(|world| !name_valid(world)) {
                return Err(format!("invalid world declarations of app {app}"));
            }
        }
        for (name, pack) in &self.packs {
            if !name_valid(name) || pack.prompt.as_ref().is_some_and(|prompt| prompt.len() > MAX_PROMPT_BYTES) {
                return Err(format!("invalid pack declaration {name}"));
            }
        }
        for (app, packs) in &self.app_packs {
            let unique: BTreeSet<_> = packs.iter().collect();
            if !name_valid(app)
                || unique.len() != packs.len()
                || packs.iter().any(|pack| !self.packs.contains_key(pack))
            {
                return Err(format!("invalid packs of app {app}"));
            }
        }
        Ok(())
    }

    /// Checks that `revision` holds every world and pack the release declares.
    /// # Errors
    /// Names the first declared world or pack the revision lacks.
    pub fn check(&self, revision: &AssetRevision) -> Result<(), String> {
        for (app, worlds) in &self.worlds {
            for world in worlds {
                if !revision.apps.get(app).is_some_and(|assets| assets.worlds.contains_key(world)) {
                    return Err(format!("the asset revision has no world {world} for app {app}"));
                }
            }
        }
        if let Some(pack) = self.packs.keys().find(|pack| !revision.packs.contains_key(*pack)) {
            return Err(format!("the asset revision has no pack {pack}"));
        }
        Ok(())
    }

    /// The packs `app`'s players hold, served from `url_prefix` followed by each pack's SHA-256. Packs `revision` lacks
    /// are skipped; [`check`](Self::check) rejects such a revision before it is deployed.
    #[must_use]
    pub fn packs_for(&self, app: &str, revision: &AssetRevision, url_prefix: &str) -> Vec<ResolvedPack> {
        let names = self.app_packs.get(app).map(Vec::as_slice).unwrap_or_default();
        names
            .iter()
            .filter_map(|name| {
                let (declaration, blob) = (self.packs.get(name)?, revision.packs.get(name)?);
                Some(ResolvedPack {
                    name: name.clone(),
                    id: pack_id(name),
                    url: format!("{url_prefix}{}", blob.sha256),
                    sha1: blob.sha1.clone(),
                    required: declaration.required,
                    prompt: declaration.prompt.clone(),
                })
            })
            .collect()
    }
}

/// The UUID a pack keeps across revisions, so a client replaces its earlier version rather than stacking both: a
/// version 8 UUID from the SHA-256 of its name.
#[must_use]
pub fn pack_id(name: &str) -> [u8; 16] {
    let digest = Sha256::digest(format!("chunk/pack/{name}"));
    let mut id = [0; 16];
    id.copy_from_slice(&digest[..16]);
    id[6] = (id[6] & 0x0f) | 0x80;
    id[8] = (id[8] & 0x3f) | 0x80;
    id
}

fn name_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// A portable relative path: nonempty segments of letters, digits, `.`, `_` and `-`, none `.` or `..`.
fn path_valid(path: &str) -> bool {
    path.len() <= MAX_PATH_BYTES
        && path.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
}

fn digest(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(byte: char) -> AssetBlob {
        AssetBlob { sha256: byte.to_string().repeat(64), size: 1 }
    }

    #[test]
    fn decode_accepts_only_canonical_valid_revisions() {
        let mut revision = AssetRevision { version: ASSET_REVISION_VERSION, ..AssetRevision::default() };
        revision.shared.insert("config/game.json".into(), blob('a'));
        revision.apps.entry("arena".into()).or_default().worlds.insert("koth".into(), blob('b'));
        let bytes = revision.encode();
        assert_eq!(AssetRevision::decode(&bytes).unwrap(), revision);
        let pretty = serde_json::to_vec_pretty(&revision).unwrap();
        assert!(AssetRevision::decode(&pretty).is_err());
        revision.shared.insert("../escape".into(), blob('c'));
        assert!(AssetRevision::decode(&revision.encode()).is_err());
    }

    #[test]
    fn validate_rejects_one_digest_with_two_sizes() {
        let mut revision = AssetRevision { version: ASSET_REVISION_VERSION, ..AssetRevision::default() };
        revision.shared.insert("a.json".into(), blob('a'));
        revision.shared.insert("b.json".into(), AssetBlob { size: 2, ..blob('a') });
        assert!(revision.validate().unwrap_err().contains("different sizes"));
    }

    #[test]
    fn contracts_check_declarations_and_resolve_packs_in_order() {
        let mut revision = AssetRevision { version: ASSET_REVISION_VERSION, ..AssetRevision::default() };
        let pack =
            |byte: char| PackBlob { sha256: byte.to_string().repeat(64), sha1: byte.to_string().repeat(40), size: 1 };
        revision.packs.insert("base".into(), pack('a'));
        revision.packs.insert("ui".into(), pack('b'));
        let contract = AssetContract {
            worlds: BTreeMap::from([("arena".into(), BTreeSet::from(["koth".into()]))]),
            packs: BTreeMap::from([
                ("base".into(), PackDeclaration::default()),
                ("ui".into(), PackDeclaration { required: true, prompt: None }),
            ]),
            app_packs: BTreeMap::from([("arena".into(), vec!["base".into(), "ui".into()])]),
        };
        contract.validate().unwrap();
        assert!(contract.check(&revision).unwrap_err().contains("koth"));
        revision.apps.entry("arena".into()).or_default().worlds.insert("koth".into(), blob('c'));
        contract.check(&revision).unwrap();
        let packs = contract.packs_for("arena", &revision, "https://example.test/packs/");
        assert_eq!(packs.iter().map(|pack| pack.name.as_str()).collect::<Vec<_>>(), ["base", "ui"]);
        assert_eq!(packs[1].url, format!("https://example.test/packs/{}", "b".repeat(64)));
        assert!(packs[1].required);
        assert_eq!(packs[0].id, pack_id("base"));
        assert!(contract.packs_for("lobby", &revision, "").is_empty());
    }
}
