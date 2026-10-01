use std::{
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};
const VERSION: &str = "7.0.2";

pub(super) fn executable() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("CHUNK_TYPESCRIPT") {
        return Ok(path.into());
    }
    let executable = std::env::current_exe()?;
    let parent = executable.parent().ok_or_else(|| io::Error::other("executable directory missing"))?;
    let parent =
        if parent.file_name().is_some_and(|name| name == "deps") { parent.parent().unwrap_or(parent) } else { parent };
    let path = parent.join("toolchain/typescript").join(VERSION).join(if cfg!(windows) { "tsc.exe" } else { "tsc" });
    if !path.is_file() {
        return Err(io::Error::other(format!(
            "TypeScript {VERSION} missing. Reinstall the CLI or set CHUNK_TYPESCRIPT to its executable."
        )));
    }
    Ok(path)
}

pub(super) fn check(files: &[&Path], output: &Path) -> io::Result<()> {
    let compiler = executable()?;
    let version = Command::new(&compiler).arg("--version").output()?;
    if !version.status.success() || String::from_utf8_lossy(&version.stdout).trim() != format!("Version {VERSION}") {
        return Err(io::Error::other(format!("requires native TypeScript {VERSION}")));
    }
    let config = output.join("tsconfig.json");
    fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "compilerOptions": {
                "target": "ES2023", "module": "ESNext", "moduleResolution": "Bundler",
                "strict": true, "exactOptionalPropertyTypes": true, "noEmit": true, "allowImportingTsExtensions": true,
                "types": [], "lib": ["ES2023"]
            },
            "files": files
        }))
        .map_err(io::Error::other)?,
    )?;
    let result = Command::new(compiler).args(["--pretty", "false", "--noEmit", "--project"]).arg(config).output()?;
    if !result.status.success() {
        return Err(io::Error::other(format!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    Ok(())
}
