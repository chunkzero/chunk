//! `chunk auth`: the device login, its status and logout.

use std::{
    io::{self, IsTerminal},
    time::Duration,
};

use chunk_management::{
    Code,
    v1::{GetCurrentPrincipalRequest, LoginState, PollLoginRequest, RevokeTokenRequest, StartLoginRequest},
};
use clap::{Args, Subcommand};
use url::Url;

use super::{
    api_error, client,
    config::{self, Config, Secret, Target, parse_url},
};

#[derive(Subcommand)]
pub(crate) enum Auth {
    /// Log in to Chunk Cloud or a self-hosted platform, approving the login in its dashboard.
    Login(Login),
    /// Show the platform and who you are logged in as.
    Status,
    /// Revoke this CLI's token and forget it.
    Logout,
}

#[derive(Args)]
pub(crate) struct Login {
    /// Use Chunk Cloud.
    #[arg(long, conflicts_with = "url")]
    cloud: bool,
    /// Use a self-hosted platform's URL.
    #[arg(long, value_parser = parse_url)]
    url: Option<Url>,
}

pub(super) async fn run(command: Auth) -> io::Result<()> {
    match command {
        Auth::Login(options) => login(options).await,
        Auth::Status => status().await,
        Auth::Logout => logout().await,
    }
}

async fn login(options: Login) -> io::Result<()> {
    let target = choose_target(options)?;
    let client = client(&target);
    let started =
        client.start_login(&StartLoginRequest { client_name: "chunk CLI".into() }).await.map_err(api_error)?;
    cliclack::note(
        "Approve this login",
        format!("Open {}\nand confirm the code {}", started.verification_url, started.user_code),
    )?;
    let interval = started
        .poll_interval
        .and_then(|interval| Duration::try_from(interval).ok())
        .unwrap_or(Duration::from_secs(5))
        .clamp(Duration::from_secs(1), Duration::from_secs(30));
    cliclack::log::info("Waiting for approval…")?;
    let request = PollLoginRequest { login_id: started.login_id };
    let secret = loop {
        tokio::time::sleep(interval).await;
        let polled = match client.poll_login(&request).await {
            Ok(polled) => polled,
            Err(error) if error.code() == Code::NotFound => return Err(expired()),
            Err(error) => return Err(poll_failed(&error)),
        };
        match polled.state() {
            LoginState::Pending => {}
            LoginState::Approved if !polled.secret.is_empty() => break Secret::new(polled.secret),
            _ => return Err(expired()),
        }
    };
    let principal = client
        .clone()
        .with_token(secret.expose())
        .get_current_principal(&GetCurrentPrincipalRequest {})
        .await
        .map_err(api_error)?
        .principal
        .unwrap_or_default();
    config::save(&Config { target: target.clone(), token: Some(secret) })?;
    cliclack::log::success(format!("Logged in to {target} as {}", principal.display_name))?;
    warn_overrides()
}

/// A failed poll's error as its code alone: the server's diagnostic may quote the login ID, which collects the token
/// once the login is approved.
pub(super) fn poll_failed(error: &chunk_management::Error) -> io::Error {
    io::Error::other(format!("Checking the login failed ({}); run `chunk auth login` again.", error.code()))
}

fn expired() -> io::Error {
    io::Error::other("The login expired before it was approved; run `chunk auth login` again.")
}

fn choose_target(options: Login) -> io::Result<Target> {
    if let Some(url) = options.url {
        return Ok(Target::Custom(url));
    }
    if options.cloud {
        return Ok(Target::Cloud);
    }
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err(io::Error::other("Use --cloud or --url URL without a terminal."));
    }
    cliclack::intro("Log in")?;
    let cloud = cliclack::select("Platform")
        .item(true, "Chunk Cloud", "Managed hosting")
        .item(false, "Custom platform", "Self-hosted")
        .interact()?;
    if cloud {
        return Ok(Target::Cloud);
    }
    let value: String = cliclack::input("API URL")
        .placeholder("https://chunk.example.com")
        .validate(|value: &String| parse_url(value).map(|_| ()))
        .interact()?;
    parse_url(&value).map(Target::Custom).map_err(io::Error::other)
}

async fn status() -> io::Result<()> {
    let credentials = config::credentials()?;
    let target = &credentials.target;
    let Some(token) = credentials.token else {
        return cliclack::log::info(format!("{target} · not logged in"));
    };
    let client = client(target).with_token(token.expose());
    let current = client.get_current_principal(&GetCurrentPrincipalRequest {}).await.map_err(api_error)?;
    let name = current.principal.unwrap_or_default().display_name;
    let source = if credentials.from_env { " with CHUNK_TOKEN" } else { "" };
    cliclack::log::info(format!("{target} · logged in as {name}{source}"))
}

/// Forgets the saved token, then revokes it; never `CHUNK_TOKEN`. A revocation that fails or times out only warns.
async fn logout() -> io::Result<()> {
    let Config { target, token } = config::load()?;
    let Some(token) = token else {
        cliclack::log::info(format!("Not logged in to {target}."))?;
        return warn_overrides();
    };
    config::forget(&token)?;
    let client = client(&target).with_token(token.expose());
    let revoked = async {
        let current = client.get_current_principal(&GetCurrentPrincipalRequest {}).await?;
        if let Some(token) = current.token {
            client.revoke_token(&RevokeTokenRequest { token_id: token.id }).await?;
        }
        Ok::<_, chunk_management::Error>(())
    }
    .await;
    match revoked {
        Ok(()) => cliclack::log::success(format!("Logged out of {target}"))?,
        Err(error) if error.code() == Code::Unauthenticated => {
            cliclack::log::success(format!("Logged out of {target}; its token was already invalid"))?;
        }
        Err(error) => {
            cliclack::log::warning(format!("Forgot the token, but {target} could not revoke it: {error}"))?;
        }
    }
    warn_overrides()
}

fn warn_overrides() -> io::Result<()> {
    for name in ["CHUNK_API_URL", "CHUNK_TOKEN"] {
        if std::env::var_os(name).is_some_and(|value| !value.is_empty()) {
            cliclack::log::warning(format!("{name} overrides your saved login."))?;
        }
    }
    Ok(())
}
