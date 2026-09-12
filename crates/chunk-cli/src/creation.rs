use std::{fs, io, path::PathBuf};

use clap::{Args, ValueEnum};

#[derive(Args)]
pub(crate) struct Options {
    /// New project directory (must not already exist).
    directory: PathBuf,
    /// Gameplay source language.
    #[arg(long, value_enum, default_value = "kotlin")]
    language: Language,
    /// Chunk checkout supplying the Gradle wrapper, plugin and runtime libraries.
    #[arg(long, env = "CHUNK_SOURCE")]
    chunk_source: PathBuf,
}

#[derive(Clone, Copy, ValueEnum)]
enum Language {
    Java,
    Kotlin,
}

const COMMON: &[(&str, &str)] = &[
    ("chunk.toml", include_str!("../templates/common/chunk.toml")),
    (".gitignore", include_str!("../templates/common/gitignore")),
    ("README.md", include_str!("../templates/common/README.md")),
    ("server/schema/index.ts", include_str!("../templates/common/schema.ts")),
    ("server/greetings.ts", include_str!("../templates/common/greetings.ts")),
    ("server/proxy.ts", include_str!("../templates/common/proxy.ts")),
    ("apps/lobby/app.toml", ""),
];
const WRAPPER: &[&str] =
    &["gradlew", "gradlew.bat", "gradle/wrapper/gradle-wrapper.jar", "gradle/wrapper/gradle-wrapper.properties"];

pub(crate) fn run(options: &Options) -> io::Result<()> {
    let executable = std::env::current_exe()?;
    create(options, &executable)?;
    cliclack::log::success(format!("Created project → {}", options.directory.display()))?;
    cliclack::log::info("Next: run chunk codegen, chunk build, then chunk dev in the project directory")
}

fn create(options: &Options, executable: &std::path::Path) -> io::Result<()> {
    let source = options.chunk_source.canonicalize()?;
    for name in WRAPPER.iter().copied().chain([
        "settings.gradle.kts",
        "jvm/gradle-plugin/settings.gradle.kts",
        "jvm/runtime-minestom/build.gradle.kts",
    ]) {
        if !source.join(name).is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Chunk source checkout is missing {name}: {}", source.display()),
            ));
        }
    }
    let (plugin, settings, app_build, gameplay, gameplay_path) = match options.language {
        Language::Java => (
            "dev.chunkzero.chunk",
            include_str!("../templates/java/settings.gradle.kts"),
            include_str!("../templates/java/app.gradle.kts"),
            include_str!("../templates/java/Lobby.java"),
            "apps/lobby/src/main/java/example/Lobby.java",
        ),
        Language::Kotlin => (
            "dev.chunkzero.chunk.kotlin",
            include_str!("../templates/kotlin/settings.gradle.kts"),
            include_str!("../templates/kotlin/app.gradle.kts"),
            include_str!("../templates/kotlin/Lobby.kt"),
            "apps/lobby/src/main/kotlin/example/Lobby.kt",
        ),
    };
    let properties = format!(
        "chunk.source={}\nchunk.executable={}\norg.gradle.caching=true\norg.gradle.configuration-cache=true\norg.gradle.jvmargs=-Xmx2g\n",
        property(&source)?,
        property(executable)?,
    );
    let root_build = format!(
        "plugins {{\n    id(\"{plugin}\")\n}}\n\njava {{ toolchain.languageVersion = JavaLanguageVersion.of(25) }}\n"
    );

    // Reserve a new directory before writing any files; existing projects are never merged or replaced.
    fs::create_dir(&options.directory).map_err(|error| {
        io::Error::new(error.kind(), format!("cannot create project {}: {error}", options.directory.display()))
    })?;
    for (name, content) in COMMON.iter().copied().chain([
        ("settings.gradle.kts", settings),
        ("build.gradle.kts", &root_build),
        ("gradle.properties", &properties),
        ("apps/lobby/build.gradle.kts", app_build),
        (gameplay_path, gameplay),
    ]) {
        let target = options.directory.join(name);
        fs::create_dir_all(target.parent().expect("template file has a parent"))?;
        fs::write(target, content)?;
    }
    for name in WRAPPER {
        let target = options.directory.join(name);
        fs::create_dir_all(target.parent().expect("wrapper file has a parent"))?;
        fs::copy(source.join(name), target)?;
    }
    Ok(())
}

// Gradle reads Java properties as ISO-8859-1, with backslash escapes and UTF-16 Unicode escapes.
fn property(path: &std::path::Path) -> io::Result<String> {
    use std::fmt::Write;

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
