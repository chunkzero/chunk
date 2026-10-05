use std::{
    fmt::Write,
    fs, io,
    path::{Path, PathBuf},
};

use clap::{Args, ValueEnum};

mod files;
mod toolchain;

#[derive(Args)]
pub(crate) struct Options {
    /// New or empty project directory; symlinks are unsupported.
    directory: PathBuf,
    /// Gameplay source language.
    #[arg(long, value_enum, default_value = "kotlin")]
    language: Language,
    /// Use a Chunk checkout instead of the embedded toolchain (framework development).
    #[arg(long, env = "CHUNK_SOURCE")]
    chunk_source: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Language {
    Java,
    Kotlin,
}

const COMMON: &[(&str, &str)] = &[
    ("chunk.toml", include_str!("../templates/common/chunk.toml")),
    (".gitignore", include_str!("../templates/common/gitignore")),
    ("server/schema/index.ts", include_str!("../templates/common/schema.ts")),
    ("server/greetings.ts", include_str!("../templates/common/greetings.ts")),
    ("apps/scope.ts", include_str!("../templates/common/scope.ts")),
    ("apps/lobby/app.ts", include_str!("../templates/common/app.ts")),
];
const WRAPPER: &[(&str, &[u8])] = &[
    ("gradlew", include_bytes!("../../../gradlew")),
    ("gradlew.bat", include_bytes!("../../../gradlew.bat")),
    ("gradle/wrapper/gradle-wrapper.jar", include_bytes!("../../../gradle/wrapper/gradle-wrapper.jar")),
    ("gradle/wrapper/gradle-wrapper.properties", include_bytes!("../../../gradle/wrapper/gradle-wrapper.properties")),
];

pub(crate) fn run(options: &Options) -> io::Result<()> {
    let executable = std::env::current_exe()?;
    create(options, &executable)?;
    cliclack::log::success(format!("Created project → {}", options.directory.display()))?;
    let executable = shell(&executable)?;
    cliclack::log::info(format!(
        "Next:\n  cd {}\n  {executable} codegen\n  {executable} build\n  {executable} dev",
        shell(&options.directory.canonicalize()?)?,
    ))
}

fn create(options: &Options, executable: &Path) -> io::Result<()> {
    files::validate(&options.directory)?;
    let toolchain = toolchain::Toolchain::resolve(options.chunk_source.as_deref())?;
    let directory = if options.directory.exists() {
        options.directory.canonicalize()?
    } else {
        std::path::absolute(&options.directory)?
    };
    let name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "project directory must have a UTF-8 name"))?;
    let (plugin, settings, app_build, gameplay, gameplay_path) = match options.language {
        Language::Java => (
            "com.chunkzero.chunk",
            include_str!("../templates/java/settings.gradle.kts"),
            include_str!("../templates/java/app.gradle.kts"),
            include_str!("../templates/java/Lobby.java"),
            "apps/lobby/src/main/java/example/Lobby.java",
        ),
        Language::Kotlin => (
            "com.chunkzero.chunk.kotlin",
            include_str!("../templates/kotlin/settings.gradle.kts"),
            include_str!("../templates/kotlin/app.gradle.kts"),
            include_str!("../templates/kotlin/Lobby.kt"),
            "apps/lobby/src/main/kotlin/example/Lobby.kt",
        ),
    };
    let mut properties = format!(
        "chunk.executable={}\norg.gradle.caching=true\norg.gradle.configuration-cache=true\norg.gradle.jvmargs=-Xmx2g\n",
        property(executable)?,
    );
    if let Some(source) = &toolchain.source {
        writeln!(properties, "chunk.source={}", property(source)?).expect("writing to String cannot fail");
    }
    let settings = settings
        .replace("\"@CHUNK_VERSION@\"", &kotlin(&toolchain.versions.chunk))
        .replace("\"@KOTLIN_VERSION@\"", &kotlin(&toolchain.versions.kotlin))
        .replace("\"@FOOJAY_VERSION@\"", &kotlin(&toolchain.versions.foojay))
        .replace("\"@PROJECT_NAME@\"", &kotlin(name));
    let readme = include_str!("../templates/common/README.md")
        .replace("@CHUNK_COMMAND@", &shell(executable)?)
        .replace("@PROJECT_NAME@", name);
    let root_build = format!(
        "plugins {{\n    id(\"{plugin}\")\n}}\n\njava {{ toolchain.languageVersion = JavaLanguageVersion.of(25) }}\n"
    );

    let staging = tempfile::tempdir()?;
    for (name, content) in COMMON.iter().copied().chain([
        ("settings.gradle.kts", settings.as_str()),
        ("README.md", &readme),
        ("build.gradle.kts", &root_build),
        ("gradle.properties", &properties),
        ("apps/lobby/build.gradle.kts", app_build),
        (gameplay_path, gameplay),
    ]) {
        let target = staging.path().join(name);
        fs::create_dir_all(target.parent().expect("template file has a parent"))?;
        fs::write(target, content)?;
    }
    for &(name, contents) in WRAPPER {
        let target = staging.path().join(name);
        fs::create_dir_all(target.parent().expect("wrapper file has a parent"))?;
        if let Some(source) = &toolchain.source {
            fs::copy(source.join(name), &target)?;
        } else {
            fs::write(&target, contents)?;
        }
        #[cfg(unix)]
        if name == "gradlew" {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
        }
    }
    files::install(staging.path(), &directory)
}

fn kotlin(value: &str) -> String {
    serde_json::to_string(value).expect("a string can be encoded as JSON").replace('$', "\\$")
}

fn shell(path: &Path) -> io::Result<String> {
    let path = path.to_str().ok_or_else(|| io::Error::other("project toolchain paths must be UTF-8"))?;
    Ok(format!("'{}'", path.replace('\'', "'\"'\"'")))
}

// Gradle reads Java properties as ISO-8859-1, with backslash escapes and UTF-16 Unicode escapes.
fn property(path: &Path) -> io::Result<String> {
    let path = path.to_str().ok_or_else(|| io::Error::other("project toolchain paths must be UTF-8"))?;
    let mut result = String::new();
    for unit in path.encode_utf16() {
        match unit {
            0x20 | 0x5c | 0x3d | 0x3a | 0x23 | 0x21 => {
                result.push('\\');
                result.push(char::from_u32(u32::from(unit)).expect("ASCII character"));
            }
            0x21..=0x7e => result.push(char::from_u32(u32::from(unit)).expect("ASCII character")),
            _ => write!(result, "\\u{unit:04x}").expect("writing to String cannot fail"),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests;
