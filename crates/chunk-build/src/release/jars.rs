use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Cursor, Read},
    path::Path,
};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use zip::{ZipArchive, read::ZipFile};

const APP_METADATA: &str = "META-INF/chunk/app.json";
const PROVIDER: &str = "META-INF/services/dev.chunkzero.runtime.SessionProvider";

#[derive(Default)]
pub(super) struct Classpath {
    classes: BTreeMap<String, ([u8; 32], String)>,
    expanded_bytes: u64,
    entries: usize,
}

impl Classpath {
    pub fn add(&mut self, bytes: &[u8], label: &str, java: u32, app: Option<&str>) -> io::Result<()> {
        let mut jar = ZipArchive::new(Cursor::new(bytes)).map_err(io::Error::other)?;
        self.entries += jar.len();
        if self.entries > 200_000 {
            return Err(io::Error::other("JVM classpath exceeds 200000 archive entries"));
        }
        let multi_release = match jar.by_name("META-INF/MANIFEST.MF") {
            Ok(file) => manifest_is_multi_release(&read_entry(file, 65_536)?)?,
            Err(zip::result::ZipError::FileNotFound) => false,
            Err(error) => return Err(io::Error::other(error)),
        };
        let mut names = BTreeSet::new();
        let mut classes = BTreeMap::<String, (u32, [u8; 32])>::new();
        let mut metadata = None;
        let mut provider = None;
        for index in 0..jar.len() {
            let file = jar.by_index(index).map_err(io::Error::other)?;
            let name = file.name().to_owned();
            if !names.insert(name.clone()) || name.len() > 4096 || file.enclosed_name().is_none() {
                return Err(io::Error::other(format!("{label} contains an invalid or duplicate JAR entry")));
            }
            if file.is_dir() {
                continue;
            }
            if file.is_symlink() {
                return Err(io::Error::other("JAR symlinks are unsupported"));
            }
            match name.as_str() {
                APP_METADATA => metadata = Some(read_entry(file, 65_536)?),
                PROVIDER => provider = Some(read_entry(file, 65_536)?),
                _ => {
                    if let Some((version, class)) = effective_class(&name, multi_release, java)? {
                        self.expanded_bytes += file.size();
                        if self.expanded_bytes > 512 * 1024 * 1024 {
                            return Err(io::Error::other("expanded JVM classes exceed local size limits"));
                        }
                        let bytes = read_entry(file, 16 * 1024 * 1024)?;
                        validate_bytecode(&bytes, java, &name)?;
                        let hash = Sha256::digest(&bytes).into();
                        match classes.get(class) {
                            Some((previous, _)) if *previous >= version => {}
                            _ => {
                                classes.insert(class.into(), (version, hash));
                            }
                        }
                    }
                }
            }
        }
        validate_registration(app, metadata.as_deref(), provider.as_deref(), &names)?;
        for (name, (_, hash)) in classes {
            if let Some((previous, owner)) = self.classes.get(&name) {
                if previous != &hash {
                    return Err(io::Error::other(format!("conflicting class {name} in {owner} and {label}")));
                }
            } else {
                self.classes.insert(name, (hash, label.into()));
            }
        }
        Ok(())
    }
}

fn read_entry(mut file: ZipFile<'_, Cursor<&[u8]>>, limit: u64) -> io::Result<Vec<u8>> {
    if file.size() > limit {
        return Err(io::Error::other("JAR entry exceeds local size limits"));
    }
    let mut bytes = Vec::new();
    (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("JAR entry exceeds local size limits"));
    }
    Ok(bytes)
}

fn manifest_is_multi_release(bytes: &[u8]) -> io::Result<bool> {
    let source = std::str::from_utf8(bytes).map_err(io::Error::other)?;
    let mut attributes: Vec<String> = Vec::new();
    for line in source.lines().take_while(|line| !line.is_empty()) {
        if let Some(continuation) = line.strip_prefix(' ') {
            if let Some(previous) = attributes.last_mut() {
                previous.push_str(continuation);
            }
        } else {
            attributes.push(line.into());
        }
    }
    Ok(attributes.iter().any(|line| {
        line.split_once(':').is_some_and(|(key, value)| {
            key.eq_ignore_ascii_case("Multi-Release") && value.trim().eq_ignore_ascii_case("true")
        })
    }))
}

fn effective_class(name: &str, multi_release: bool, java: u32) -> io::Result<Option<(u32, &str)>> {
    if Path::new(name).extension().is_none_or(|extension| extension != "class") {
        return Ok(None);
    }
    let (version, class) = if let Some(rest) = name.strip_prefix("META-INF/versions/") {
        if !multi_release {
            return Ok(None);
        }
        let (version, class) =
            rest.split_once('/').ok_or_else(|| io::Error::other("invalid multi-release class path"))?;
        let version: u32 = version.parse().map_err(io::Error::other)?;
        if version < 9 || version > java {
            return Ok(None);
        }
        (version, class)
    } else if name.starts_with("META-INF/") {
        return Ok(None);
    } else {
        (0, name)
    };
    if class == "module-info.class" {
        return Ok(None);
    }
    Ok(Some((version, class)))
}

fn validate_bytecode(bytes: &[u8], java: u32, name: &str) -> io::Result<()> {
    if bytes.len() < 8 || bytes[..4] != [0xca, 0xfe, 0xba, 0xbe] {
        return Err(io::Error::other(format!("invalid class header for {name}")));
    }
    let minor = u16::from_be_bytes([bytes[4], bytes[5]]);
    let major = u16::from_be_bytes([bytes[6], bytes[7]]);
    if minor == u16::MAX || u32::from(major) > java + 44 {
        return Err(io::Error::other(format!("{name} requires incompatible or preview Java bytecode")));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppMetadata {
    version: u32,
    id: String,
}

fn validate_registration(
    app: Option<&str>,
    metadata: Option<&[u8]>,
    provider: Option<&[u8]>,
    names: &BTreeSet<String>,
) -> io::Result<()> {
    let Some(app) = app else {
        if metadata.is_some() || provider.is_some() {
            return Err(io::Error::other("dependency JAR contains an undeclared app registration"));
        }
        return Ok(());
    };
    let metadata: AppMetadata =
        serde_json::from_slice(metadata.ok_or_else(|| io::Error::other("app JAR is missing META-INF/chunk/app.json"))?)
            .map_err(io::Error::other)?;
    if metadata.version != 1 || metadata.id != app {
        return Err(io::Error::other(format!("app JAR identity does not match {app:?}")));
    }
    let provider = std::str::from_utf8(
        provider.ok_or_else(|| io::Error::other("app JAR is missing its SessionProvider service entry"))?,
    )
    .map_err(io::Error::other)?;
    let providers: Vec<_> = provider
        .lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .filter(|line| !line.is_empty())
        .collect();
    if providers.len() != 1
        || !class_name(providers[0])
        || !names.contains(&format!("{}.class", providers[0].replace('.', "/")))
    {
        return Err(io::Error::other("app JAR requires one SessionProvider class in that JAR"));
    }
    Ok(())
}

fn class_name(name: &str) -> bool {
    name.split('.').all(|part| {
        part.chars().next().is_some_and(|ch| ch.is_alphabetic() || ch == '_' || ch == '$')
            && part.chars().all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$')
    })
}
