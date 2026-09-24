use std::{
    io::{Cursor, Read},
    path::Path,
};

use sha2::{Digest, Sha256};
use zip::ZipArchive;

use crate::{Error, Result};

const MANIFEST_LIMIT: u64 = 65_536;

/// Verifies every manifest `Class-Path` entry of the launched `jar`, whose `bytes` already matched the app digest.
/// Each entry must resolve inside `distribution` to a file named by its own SHA-256 digest, as `chunk dev` launchers
/// reference `libs/<sha256>.jar`, so no JAR the JVM loads can change under the app's artifact identity.
pub(super) fn verify(distribution: &Path, jar: &Path, bytes: &[u8]) -> Result<()> {
    let mismatch = || Error::Invalid("app classpath digest mismatch");
    let invalid = || Error::Invalid("app artifact is not a readable JAR");
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(|_| invalid())?;
    let mut manifest = String::new();
    match archive.by_name("META-INF/MANIFEST.MF") {
        Ok(entry) => {
            entry.take(MANIFEST_LIMIT + 1).read_to_string(&mut manifest).map_err(|_| invalid())?;
        }
        Err(zip::result::ZipError::FileNotFound) => return Ok(()),
        Err(_) => return Err(invalid()),
    }
    if manifest.len() as u64 > MANIFEST_LIMIT {
        return Err(invalid());
    }
    let directory = jar.parent().ok_or_else(mismatch)?;
    for entry in class_path(&manifest) {
        if entry.starts_with('/') || entry.contains([':', '%', '\\']) {
            return Err(mismatch());
        }
        let path = directory.join(&entry).canonicalize().map_err(|_| mismatch())?;
        let digest = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".jar"))
            .ok_or_else(mismatch)?;
        if !path.starts_with(distribution) || format!("{:x}", Sha256::digest(std::fs::read(&path)?)) != digest {
            return Err(mismatch());
        }
    }
    Ok(())
}

/// Entries of the main-section `Class-Path` attribute, with continuation lines joined.
fn class_path(manifest: &str) -> Vec<String> {
    let mut attributes: Vec<String> = Vec::new();
    for line in manifest.lines().take_while(|line| !line.is_empty()) {
        if let Some(continuation) = line.strip_prefix(' ') {
            if let Some(previous) = attributes.last_mut() {
                previous.push_str(continuation);
            }
        } else {
            attributes.push(line.into());
        }
    }
    attributes
        .iter()
        .filter_map(|attribute| attribute.split_once(':'))
        .filter(|(key, _)| key.eq_ignore_ascii_case("Class-Path"))
        .flat_map(|(_, value)| value.split_whitespace().map(String::from))
        .collect()
}
