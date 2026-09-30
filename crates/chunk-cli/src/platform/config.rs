//! The saved platform and token, and the environment variables that override them.

use std::{
    fmt, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use url::Url;

/// Chunk Cloud's management API.
pub(super) const CLOUD_URL: &str = "https://api.chunkzero.com";

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Debug)]
#[serde(tag = "kind", content = "url", rename_all = "snake_case")]
pub(super) enum Target {
    #[default]
    Cloud,
    Custom(Url),
}

impl Target {
    pub(super) fn url(&self) -> &str {
        match self {
            Self::Cloud => CLOUD_URL,
            Self::Custom(url) => url.as_str(),
        }
    }

    /// The URL without a trailing slash, equal for targets that reach the same API.
    fn endpoint(&self) -> &str {
        self.url().trim_end_matches('/')
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cloud => f.write_str("Chunk Cloud"),
            Self::Custom(url) => url.fmt(f),
        }
    }
}

/// A token's secret, which never shows in debug output.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub(super) struct Secret(String);

impl Secret {
    pub(super) fn new(secret: String) -> Self {
        Self(secret)
    }

    pub(super) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// What `chunk auth login` saves: the platform, and the token it issued, which only ever goes to that platform.
#[derive(Default, Serialize, Deserialize, PartialEq, Debug)]
pub(super) struct Config {
    #[serde(default)]
    pub target: Target,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<Secret>,
}

/// The platform to call and the token to call it with.
pub(super) struct Credentials {
    pub target: Target,
    pub token: Option<Secret>,
    /// Whether the token came from `CHUNK_TOKEN`.
    pub from_env: bool,
}

pub(super) fn parse_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|_| "expected an absolute HTTP(S) platform URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("platform URL must use HTTP(S) without credentials, query, or fragment".into());
    }
    Ok(url)
}

fn path() -> io::Result<PathBuf> {
    let directory = match std::env::var_os("CHUNK_CONFIG_DIR") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        Some(_) => return Err(io::Error::other("CHUNK_CONFIG_DIR must not be empty")),
        None => dirs::config_dir()
            .ok_or_else(|| io::Error::other("cannot locate CLI configuration directory"))?
            .join("chunk"),
    };
    Ok(directory.join("config.json"))
}

pub(super) fn load() -> io::Result<Config> {
    load_from(&path()?)
}

pub(super) fn save(config: &Config) -> io::Result<()> {
    save_to(&path()?, config)
}

pub(super) fn load_from(path: &Path) -> io::Result<Config> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    make_private(path)?;
    let config: Config = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if let Target::Custom(url) = &config.target {
        parse_url(url.as_str()).map_err(io::Error::other)?;
    }
    Ok(config)
}

/// Gives a saved login the mode 0600 `save_to` creates it with, so only its owner can read it.
#[cfg(unix)]
fn make_private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if std::fs::metadata(path)?.permissions().mode() & 0o777 == 0o600 {
        return Ok(());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
        let message = format!("{} must be private to you (mode 0600), and changing it failed: {error}", path.display());
        io::Error::new(error.kind(), message)
    })
}

/// Forgets the saved token if it is still `token`, so a login since it was loaded stays.
pub(super) fn forget(token: &Secret) -> io::Result<()> {
    let path = path()?;
    let mut config = load_from(&path)?;
    if config.token.as_ref() == Some(token) {
        config.token = None;
        save_to(&path, &config)?;
    }
    Ok(())
}

/// Replaces the file atomically. The temporary file it renames is created readable by its owner alone.
pub(super) fn save_to(path: &Path, config: &Config) -> io::Result<()> {
    let directory = path.parent().expect("the configuration file has a parent");
    std::fs::create_dir_all(directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer_pretty(&mut file, config).map_err(io::Error::other)?;
    file.persist(path).map_err(io::Error::other)?;
    Ok(())
}

/// The saved credentials, overridden by `CHUNK_API_URL` and `CHUNK_TOKEN`.
pub(super) fn credentials() -> io::Result<Credentials> {
    let url = chunk_service::optional::<String>("CHUNK_API_URL")?;
    let token = chunk_service::optional::<String>("CHUNK_TOKEN")?.filter(|token| !token.is_empty());
    resolve(url.as_deref(), token, load)
}

/// `CHUNK_TOKEN` beats the saved token, and the saved token goes only to the platform it was issued by.
pub(super) fn resolve(
    url: Option<&str>,
    token: Option<String>,
    saved: impl FnOnce() -> io::Result<Config>,
) -> io::Result<Credentials> {
    let target = url.map(|url| parse_url(url).map(Target::Custom).map_err(io::Error::other)).transpose()?;
    if let Some(token) = token {
        let target = match target {
            Some(target) => target,
            None => saved()?.target,
        };
        return Ok(Credentials { target, token: Some(Secret(token)), from_env: true });
    }
    let saved = saved()?;
    Ok(match target {
        Some(target) if target.endpoint() != saved.target.endpoint() => {
            Credentials { target, token: None, from_env: false }
        }
        _ => Credentials { target: saved.target, token: saved.token, from_env: false },
    })
}
