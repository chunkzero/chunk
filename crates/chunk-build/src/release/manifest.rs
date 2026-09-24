use std::{
    collections::BTreeMap,
    io::{self, Cursor, Read},
};

use serde::de::DeserializeOwned;
use zip::ZipArchive;

pub(super) type Archive<'a> = ZipArchive<Cursor<&'a [u8]>>;

const MANIFEST_LIMIT: u64 = 2 * 1024 * 1024;

/// A `META-INF/chunk/*.json` manifest listing the descriptors one app JAR packages.
pub(super) trait Manifest: DeserializeOwned {
    type Item;
    const FILE: &'static str;
    const LABEL: &'static str;
    fn parts(self) -> (u32, String, Vec<Self::Item>);
}

pub(super) fn read_registration(archive: &mut Archive<'_>, service: &str) -> io::Result<String> {
    let mut registration = String::new();
    archive
        .by_name(&format!("META-INF/services/{service}"))
        .map_err(io::Error::other)?
        .take(65_537)
        .read_to_string(&mut registration)?;
    Ok(registration)
}

/// Compares the packaged descriptors to `expected`; `item` validates one entry and returns its key and
/// declaration, or `None` when it is malformed.
pub(super) fn validate<M: Manifest, K: Ord, V: PartialEq>(
    bytes: &[u8],
    app: &str,
    expected: &BTreeMap<K, V>,
    mut item: impl FnMut(&mut Archive<'_>, M::Item) -> io::Result<Option<(K, V)>>,
    finish: impl FnOnce(&mut Archive<'_>, &BTreeMap<K, V>) -> io::Result<()>,
) -> io::Result<()> {
    let label = M::LABEL;
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(io::Error::other)?;
    let manifest = match archive.by_name(M::FILE) {
        Ok(mut entry) => {
            if entry.size() > MANIFEST_LIMIT {
                return Err(io::Error::other(format!("{label} manifest size limit")));
            }
            let mut bytes = Vec::new();
            (&mut entry).take(MANIFEST_LIMIT + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MANIFEST_LIMIT {
                return Err(io::Error::other(format!("{label} manifest size limit")));
            }
            Some(serde_json::from_slice::<M>(&bytes).map_err(io::Error::other)?)
        }
        Err(zip::result::ZipError::FileNotFound) if expected.is_empty() => None,
        Err(_) => return Err(io::Error::other(format!("missing {label} manifest"))),
    };
    let Some(manifest) = manifest else {
        return Ok(());
    };
    let (version, packaged_app, items) = manifest.parts();
    if version != 1 || packaged_app != app {
        return Err(io::Error::other(format!("{label} manifest identity mismatch")));
    }
    let mut actual = BTreeMap::new();
    for packaged in items {
        let (key, declaration) =
            item(&mut archive, packaged)?.ok_or_else(|| io::Error::other(format!("invalid packaged {label}")))?;
        if actual.insert(key, declaration).is_some() {
            return Err(io::Error::other(format!("duplicate packaged {label}")));
        }
    }
    if actual != *expected {
        return Err(io::Error::other(format!("packaged {label}s differ from backend contract")));
    }
    finish(&mut archive, &actual)
}
