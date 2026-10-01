//! `chunk secrets`: an environment's secret values, which backend actions read through `ctx.env`.

use std::io::{self, IsTerminal, Read as _};

use chunk_management::{
    Client,
    v1::{DeleteSecretRequest, ListSecretsRequest, Secret, SetSecretRequest},
};
use clap::{Args, Subcommand};

use super::{
    EnvironmentArgs, Session, all, api_error,
    resources::{request_id, table, time},
};

#[derive(Args)]
pub(crate) struct Secrets {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Set a secret, reading its value from a hidden prompt or, without a terminal, from stdin.
    Put {
        /// Letters, digits and underscores, not starting with a digit.
        name: String,
        #[command(flatten)]
        environment: EnvironmentArgs,
    },
    /// List secret names and versions; values are never shown.
    List {
        #[command(flatten)]
        environment: EnvironmentArgs,
    },
    /// Delete a secret.
    Delete {
        name: String,
        #[command(flatten)]
        environment: EnvironmentArgs,
        /// Delete without asking, as a terminal-less run must.
        #[arg(long, short)]
        yes: bool,
    },
}

pub(super) async fn run(options: Secrets) -> io::Result<()> {
    let session = Session::open()?;
    let client = &session.client;
    match options.action {
        Action::Put { name, environment } => {
            if !chunk_contract::valid_env_name(&name) {
                return Err(io::Error::other(
                    "Secret names are 1 to 128 letters, digits and underscores, not starting with a digit.",
                ));
            }
            let (_, environment) = session.environment(&environment).await?;
            let value = value(&name)?;
            let request = SetSecretRequest {
                request_id: request_id(),
                environment_id: environment.id,
                name,
                value: value.into(),
            };
            let secret = client.set_secret(&request).await.map_err(api_error)?.secret.unwrap_or_default();
            cliclack::log::success(format!(
                "Set {} version {} in {}; running deployments receive it without a redeploy",
                secret.name, secret.version, environment.name
            ))
        }
        Action::List { environment } => {
            let (_, environment) = session.environment(&environment).await?;
            let secrets = list(client, &environment.id).await?;
            table(
                ["NAME", "VERSION", "UPDATED"],
                secrets.into_iter().map(|secret| [secret.name, secret.version.to_string(), time(secret.update_time)]),
            )
        }
        Action::Delete { name, environment, yes } => {
            let (_, environment) = session.environment(&environment).await?;
            if !yes {
                if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
                    return Err(io::Error::other("Use --yes to delete a secret without a terminal."));
                }
                let confirmed = cliclack::confirm(format!("Delete secret {name} from {}?", environment.name))
                    .initial_value(false)
                    .interact()?;
                if !confirmed {
                    return cliclack::log::info("Kept the secret");
                }
            }
            let request = DeleteSecretRequest { environment_id: environment.id, name };
            client.delete_secret(&request).await.map_err(api_error)?;
            cliclack::log::success(format!("Deleted secret {} from {}", request.name, environment.name))
        }
    }
}

/// The value to set: typed into a hidden prompt, or all of stdin without its final line break.
fn value(name: &str) -> io::Result<String> {
    if io::stdin().is_terminal() {
        return cliclack::password(format!("Value of {name}")).mask('▪').interact();
    }
    let mut value = String::new();
    io::stdin().read_to_string(&mut value)?;
    let value = value.strip_suffix('\n').map_or(value.as_str(), |value| value.strip_suffix('\r').unwrap_or(value));
    if value.is_empty() {
        return Err(io::Error::other("The secret's value is empty."));
    }
    Ok(value.to_owned())
}

pub(super) async fn list(client: &Client, environment_id: &str) -> io::Result<Vec<Secret>> {
    all(|page_token| async move {
        let request = ListSecretsRequest { environment_id: environment_id.into(), page_token, page_size: 0 };
        client.list_secrets(&request).await.map(|page| (page.secrets, page.next_page_token))
    })
    .await
}
