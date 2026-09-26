use crate::read_limited;
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JvmDescriptor {
    pub version: u32,
    pub java: JavaRuntime,
    pub(super) apps: Vec<App>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JavaRuntime {
    pub version: u32,
    pub executable: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct App {
    pub id: String,
    pub jar: PathBuf,
    /// Runtime dependency JARs of a thin `chunk dev` app JAR; empty when `jar` bundles them.
    pub classpath: Vec<PathBuf>,
    pub java_version: u32,
    pub sessions: Vec<String>,
}

/// Reads the executable app inventory without running application code.
/// # Errors
/// Rejects malformed descriptors, nonabsolute paths and incompatible Java versions.
pub fn read_jvm_descriptor(path: &Path) -> io::Result<JvmDescriptor> {
    let descriptor: JvmDescriptor =
        serde_json::from_slice(&read_limited(path, 2 * 1024 * 1024)?).map_err(io::Error::other)?;
    if descriptor.version != 4
        || !(25..=100).contains(&descriptor.java.version)
        || !descriptor.java.executable.is_absolute()
    {
        return Err(io::Error::other("JVM descriptor requires version 4, Java 25–100 and an absolute executable path"));
    }
    if descriptor.apps.is_empty() || descriptor.apps.len() > 128 {
        return Err(io::Error::other("JVM descriptor requires 1–128 apps"));
    }
    for app in &descriptor.apps {
        let sessions: BTreeSet<_> = app.sessions.iter().collect();
        if sessions.is_empty()
            || sessions.len() > 128
            || sessions.len() != app.sessions.len()
            || sessions.iter().any(|id| !crate::valid_id(id))
        {
            return Err(io::Error::other("JVM descriptor requires 1–128 unique session type IDs per app"));
        }
        if !(25..=descriptor.java.version).contains(&app.java_version)
            || app.classpath.len() > 1024
            || std::iter::once(&app.jar)
                .chain(&app.classpath)
                .any(|jar| !jar.is_absolute() || jar.extension().is_none_or(|ext| ext != "jar"))
        {
            return Err(io::Error::other(format!(
                "app {:?} has incompatible Java requirements or a nonabsolute JAR path",
                app.id
            )));
        }
    }
    Ok(descriptor)
}
