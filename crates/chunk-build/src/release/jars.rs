use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io::{self, Cursor, Read},
    path::Path,
    sync::{Arc, LazyLock, Mutex, PoisonError},
};

use sha2::{Digest, Sha256};
use zip::{ZipArchive, read::ZipFile};

/// The effective classes of every JAR on one app's runtime classpath.
#[derive(Default)]
pub(super) struct Classpath {
    classes: BTreeMap<String, ([u8; 32], String)>,
    expanded_bytes: u64,
    entries: usize,
}

/// One JAR's effective classes, keyed by class entry path.
struct Scan {
    classes: BTreeMap<String, [u8; 32]>,
    expanded_bytes: u64,
    entries: usize,
}

/// A JAR's SHA-256 digest and the Java version it was scanned for.
type ScanKey = ([u8; 32], u32);

/// Scans are memoized by JAR digest and Java version, so `chunk dev` rescans only JARs whose bytes changed. The cache
/// holds at most [`CACHED_BYTES`] of retained scans and starts over when a scan would exceed that, so verifying
/// untrusted releases can't grow it without bound.
static SCANS: LazyLock<Mutex<Scans>> = LazyLock::new(Mutex::default);

const CACHED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct Scans {
    scans: HashMap<ScanKey, Arc<Scan>>,
    bytes: usize,
}

impl Scan {
    /// An upper estimate of the memory a cached scan retains: its cache record and a partly filled tree node, then
    /// each class's name, digest and share of the tree.
    fn retained(&self) -> usize {
        1024 + self.classes.keys().map(|name| name.len() + 128).sum::<usize>()
    }
}

impl Classpath {
    pub fn add(&mut self, bytes: &[u8], label: &str, java: u32) -> io::Result<()> {
        let key = (Sha256::digest(bytes).into(), java);
        let cached = SCANS.lock().unwrap_or_else(PoisonError::into_inner).scans.get(&key).cloned();
        let scan = if let Some(scan) = cached {
            scan
        } else {
            let scan = Arc::new(scan_jar(bytes, label, java)?);
            let mut cache = SCANS.lock().unwrap_or_else(PoisonError::into_inner);
            let retained = scan.retained();
            if cache.bytes + retained > CACHED_BYTES {
                *cache = Scans::default();
            }
            if retained <= CACHED_BYTES && cache.scans.insert(key, scan.clone()).is_none() {
                cache.bytes += retained;
            }
            scan
        };
        self.entries += scan.entries;
        if self.entries > 200_000 {
            return Err(io::Error::other("JVM classpath exceeds 200000 archive entries"));
        }
        self.expanded_bytes += scan.expanded_bytes;
        if self.expanded_bytes > 512 * 1024 * 1024 {
            return Err(io::Error::other("expanded JVM classes exceed local size limits"));
        }
        for (name, hash) in &scan.classes {
            if let Some((previous, owner)) = self.classes.get(name) {
                if previous != hash {
                    return Err(io::Error::other(format!("conflicting class {name} in {owner} and {label}")));
                }
            } else {
                self.classes.insert(name.clone(), (*hash, label.into()));
            }
        }
        Ok(())
    }

    pub fn contains(&self, class: &str) -> bool {
        self.classes.contains_key(&format!("{}.class", class.replace('.', "/")))
    }
}

fn scan_jar(bytes: &[u8], label: &str, java: u32) -> io::Result<Scan> {
    let mut jar = ZipArchive::new(Cursor::new(bytes)).map_err(io::Error::other)?;
    if jar.len() > 200_000 {
        return Err(io::Error::other("JVM classpath exceeds 200000 archive entries"));
    }
    let multi_release = match jar.by_name("META-INF/MANIFEST.MF") {
        Ok(file) => manifest_is_multi_release(&read_entry(file, 65_536)?)?,
        Err(zip::result::ZipError::FileNotFound) => false,
        Err(error) => return Err(io::Error::other(error)),
    };
    let mut names = BTreeSet::new();
    let mut classes = BTreeMap::<String, (u32, [u8; 32])>::new();
    let mut expanded_bytes = 0;
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
        if let Some((version, class)) = effective_class(&name, multi_release, java)? {
            expanded_bytes += file.size();
            if expanded_bytes > 512 * 1024 * 1024 {
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
    let classes = classes.into_iter().map(|(name, (_, hash))| (name, hash)).collect();
    Ok(Scan { classes, expanded_bytes, entries: jar.len() })
}

/// Reads the JAR's single `Main-Class`.
pub(super) fn main_class(bytes: &[u8]) -> io::Result<String> {
    let mut jar = ZipArchive::new(Cursor::new(bytes)).map_err(io::Error::other)?;
    let manifest = match jar.by_name("META-INF/MANIFEST.MF") {
        Ok(file) => read_entry(file, 65_536)?,
        Err(zip::result::ZipError::FileNotFound) => Vec::new(),
        Err(error) => return Err(io::Error::other(error)),
    };
    let attributes = attributes(&manifest)?;
    let mains: Vec<_> = attributes
        .iter()
        .filter_map(|line| line.split_once(':'))
        .filter(|(key, _)| key.eq_ignore_ascii_case("Main-Class"))
        .map(|(_, value)| value.trim())
        .collect();
    match mains.as_slice() {
        [main] if chunk_contract::class_name(main) => Ok((*main).into()),
        _ => Err(io::Error::other("executable requires one valid Main-Class")),
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
    Ok(attributes(bytes)?.iter().any(|line| {
        line.split_once(':').is_some_and(|(key, value)| {
            key.eq_ignore_ascii_case("Multi-Release") && value.trim().eq_ignore_ascii_case("true")
        })
    }))
}

/// Main manifest attributes with continuation lines joined.
pub(super) fn attributes(bytes: &[u8]) -> io::Result<Vec<String>> {
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
    Ok(attributes)
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
