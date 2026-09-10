use std::{
    io::{self, IsTerminal},
    path::PathBuf,
};

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Args)]
pub(crate) struct Scope {
    /// App name.
    #[arg(long, env = "CHUNK_APP")]
    app: Option<String>,
    /// Environment name.
    #[arg(long, env = "CHUNK_ENVIRONMENT")]
    environment: Option<String>,
}

#[derive(Args)]
pub(crate) struct Deploy {
    #[arg(default_value = ".")]
    project: PathBuf,
    #[command(flatten)]
    scope: Scope,
}

#[derive(Args)]
pub(crate) struct Logs {
    #[command(flatten)]
    scope: Scope,
    /// Follow new logs.
    #[arg(short, long)]
    follow: bool,
    #[arg(long)]
    deployment: Option<String>,
}

#[derive(Subcommand)]
pub(crate) enum ListCommand {
    /// List resources.
    List(Scope),
}

#[derive(Subcommand)]
pub(crate) enum Auth {
    /// Choose a platform (login coming soon).
    Login(Login),
    /// Show login status.
    Status,
    /// Show your account (coming soon).
    Whoami,
    /// Sign out (coming soon).
    Logout,
}

#[derive(Args)]
pub(crate) struct Login {
    /// Use Chunk Cloud.
    #[arg(long, conflicts_with = "url")]
    cloud: bool,
    /// Use a custom API URL.
    #[arg(long, value_parser = parse_url)]
    url: Option<Url>,
}

#[derive(Default, Serialize, Deserialize, PartialEq, Debug)]
#[serde(tag = "kind", content = "url", rename_all = "snake_case")]
enum Target {
    #[default]
    Cloud,
    Custom(Url),
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cloud => f.write_str("Chunk Cloud"),
            Self::Custom(url) => url.fmt(f),
        }
    }
}

fn parse_url(value: &str) -> Result<Url, String> {
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

fn config_path() -> io::Result<PathBuf> {
    let directory = match std::env::var_os("CHUNK_CONFIG_DIR") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        Some(_) => return Err(io::Error::other("CHUNK_CONFIG_DIR must not be empty")),
        None => dirs::config_dir()
            .ok_or_else(|| io::Error::other("cannot locate CLI configuration directory"))?
            .join("chunk"),
    };
    Ok(directory.join("target.json"))
}

fn resolve_target(override_url: Option<&str>, saved: impl FnOnce() -> io::Result<Target>) -> io::Result<Target> {
    match override_url {
        Some(value) => parse_url(value).map(Target::Custom).map_err(io::Error::other),
        None => saved(),
    }
}

fn target() -> io::Result<Target> {
    let override_url = chunk_service::optional::<String>("CHUNK_API_URL")?;
    resolve_target(override_url.as_deref(), || {
        let bytes = match std::fs::read(config_path()?) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Target::Cloud),
            Err(error) => return Err(error),
        };
        let target = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if let Target::Custom(ref url) = target {
            parse_url(url.as_str()).map_err(io::Error::other)?;
        }
        Ok(target)
    })
}

pub(crate) fn auth(command: Auth) -> io::Result<()> {
    match command {
        Auth::Login(options) => {
            let selected = if let Some(url) = options.url {
                Target::Custom(url)
            } else if options.cloud {
                Target::Cloud
            } else {
                if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
                    return Err(io::Error::other("Use --cloud or --url URL without a terminal."));
                }
                cliclack::intro("Log in")?;
                let cloud = cliclack::select("Platform")
                    .item(true, "Chunk Cloud", "Managed hosting")
                    .item(false, "Custom platform", "Self-hosted")
                    .interact()?;
                if cloud {
                    Target::Cloud
                } else {
                    let value: String = cliclack::input("API URL")
                        .placeholder("https://chunk.example.com")
                        .validate(|value: &String| parse_url(value).map(|_| ()))
                        .interact()?;
                    Target::Custom(parse_url(&value).map_err(io::Error::other)?)
                }
            };
            let path = config_path()?;
            let directory = path.parent().expect("target configuration has a parent");
            std::fs::create_dir_all(directory)?;
            let mut file = tempfile::NamedTempFile::new_in(directory)?;
            serde_json::to_writer_pretty(&mut file, &selected).map_err(io::Error::other)?;
            file.persist(path).map_err(io::Error::other)?;
            cliclack::log::success(format!("Saved: {selected}"))?;
            if std::env::var_os("CHUNK_API_URL").is_some() {
                cliclack::log::warning("CHUNK_API_URL overrides your saved platform.")?;
            }
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Login is not available yet.",
            ))
        }
        Auth::Status => cliclack::log::info(format!("{} · Login coming soon", target()?)),
        Auth::Whoami => unsupported("Account lookup"),
        Auth::Logout => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Logout is not available yet.",
        )),
    }
}

pub(crate) fn unsupported(operation: &str) -> io::Result<()> {
    target()?;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("{operation} — not available yet."),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_does_not_read_saved_target() {
        let actual = resolve_target(Some("https://custom.example/api"), || {
            panic!("must not read saved configuration")
        })
        .unwrap();
        assert_eq!(
            actual,
            Target::Custom(Url::parse("https://custom.example/api").unwrap())
        );
        assert!(resolve_target(Some(""), || Ok(Target::Cloud)).is_err());
    }

    #[test]
    fn target_urls_exclude_credentials_and_non_http_schemes() {
        for value in [
            "file:///tmp/platform",
            "https://user:secret@example.com",
            "https://example.com?token=secret",
            "https://example.com#fragment",
        ] {
            assert!(parse_url(value).is_err());
        }
        assert!(parse_url("http://localhost:8080/api").is_ok());
    }
}
