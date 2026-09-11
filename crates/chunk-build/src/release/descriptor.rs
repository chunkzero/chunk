use std::{
    io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::read_limited;

/// Local Gradle output. Paths are build inputs and never serialized into a release.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JvmDescriptor {
    pub version: u32,
    pub java: JavaRuntime,
    pub(super) apps: Vec<App>,
    pub(super) classpath: Vec<Dependency>,
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
    pub java_version: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Dependency {
    pub file: PathBuf,
    pub artifact: String,
    pub component: Component,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Component {
    Module { group: String, name: String, version: String },
    Project { build: String, path: String },
}

/// Reads the versioned local descriptor without executing Java or Gradle.
/// # Errors
/// Rejects unknown fields, invalid coordinates, nonabsolute input paths and unsupported versions.
pub fn read_jvm_descriptor(path: &Path) -> io::Result<JvmDescriptor> {
    let descriptor: JvmDescriptor =
        serde_json::from_slice(&read_limited(path, 2 * 1024 * 1024)?).map_err(io::Error::other)?;
    if descriptor.version != 1
        || !(21..=100).contains(&descriptor.java.version)
        || !descriptor.java.executable.is_absolute()
    {
        return Err(io::Error::other("JVM descriptor requires version 1, Java 21–100 and an absolute executable path"));
    }
    if descriptor.apps.is_empty() || descriptor.apps.len() > 128 || descriptor.classpath.len() > 1024 {
        return Err(io::Error::other("JVM descriptor requires 1–128 apps and at most 1024 classpath entries"));
    }
    for app in &descriptor.apps {
        if !(21..=descriptor.java.version).contains(&app.java_version) || !jar_path(&app.jar) {
            return Err(io::Error::other(format!(
                "app {:?} has incompatible Java requirements or a nonabsolute JAR path",
                app.id
            )));
        }
    }
    for dependency in &descriptor.classpath {
        if !jar_path(&dependency.file)
            || !artifact_name(&dependency.artifact)
            || dependency.file.file_name().and_then(|name| name.to_str()) != Some(dependency.artifact.as_str())
        {
            return Err(io::Error::other("classpath artifact must match its absolute JAR path's filename"));
        }
        let valid = match &dependency.component {
            Component::Module { group, name, version } => {
                [group, name, version].into_iter().all(|value| coordinate(value))
            }
            Component::Project { build, path } => gradle_path(build) && gradle_path(path),
        };
        if !valid {
            return Err(io::Error::other("invalid JVM dependency component identity"));
        }
    }
    Ok(descriptor)
}

fn jar_path(path: &Path) -> bool {
    path.is_absolute() && path.extension().is_some_and(|extension| extension == "jar")
}

fn coordinate(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"._-+".contains(&byte))
}

fn gradle_path(value: &str) -> bool {
    value == ":" || value.strip_prefix(':').is_some_and(|rest| rest.split(':').all(coordinate))
}

fn artifact_name(value: &str) -> bool {
    Path::new(value).extension().is_some_and(|extension| extension == "jar") && coordinate(value)
}
