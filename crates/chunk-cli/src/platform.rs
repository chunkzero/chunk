//! Commands against a platform's `chunk.management.v1` API: Chunk Cloud or a self-hosted install, identically.

use std::{io, time::Duration};

use chunk_management::{
    Client, Code,
    v1::{Environment, ListEnvironmentsRequest, ListProjectsRequest, Project},
};
use clap::{Args, Subcommand};

mod auth;
mod config;
mod deploy;
mod keychain;
mod resources;

use auth::{Auth, Login};
use config::Target;

/// How long a unary call may take before it fails as unavailable.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Log in to a platform, show the login, or log out.
    #[command(subcommand)]
    Auth(Auth),
    /// Alias for auth login.
    Login(Login),
    /// Build the project and deploy its release to an environment.
    Deploy(deploy::Options),
    /// List projects, or create one.
    Projects(resources::Projects),
    /// List a project's environments, or create one.
    Environments(resources::Environments),
    /// List an environment's recent deployments, newest first.
    Deployments(resources::Deployments),
    /// List the apps and session types an environment's active release runs.
    Apps(resources::Apps),
    /// Print an environment's logs.
    Logs(resources::Logs),
}

pub(crate) async fn run(command: Command) -> io::Result<()> {
    match command {
        Command::Auth(auth) => auth::run(auth).await,
        Command::Login(options) => auth::run(Auth::Login(options)).await,
        Command::Deploy(options) => deploy::run(options).await,
        Command::Projects(options) => resources::projects(options).await,
        Command::Environments(options) => resources::environments(options).await,
        Command::Deployments(options) => resources::deployments(options).await,
        Command::Apps(options) => resources::apps(options).await,
        Command::Logs(options) => resources::logs(options).await,
    }
}

#[derive(Args)]
struct ProjectArg {
    /// The project's name or ID; defaults to your only project.
    #[arg(long, global = true, env = "CHUNK_PROJECT")]
    project: Option<String>,
}

#[derive(Args)]
struct EnvironmentArgs {
    #[command(flatten)]
    project: ProjectArg,
    /// The environment's name or ID.
    #[arg(long = "env")]
    environment: String,
}

/// A client for the selected platform, carrying the caller's token.
struct Session {
    client: Client,
}

impl Session {
    fn open() -> io::Result<Self> {
        let credentials = config::credentials()?;
        let Some(token) = credentials.token else {
            return Err(io::Error::other(format!(
                "Not logged in to {}. Run `chunk auth login`, or set CHUNK_TOKEN.",
                credentials.target
            )));
        };
        Ok(Self { client: client(&credentials.target).with_token(token.expose()) })
    }

    async fn project(&self, selector: &ProjectArg) -> io::Result<Project> {
        let client = &self.client;
        let projects = all(|page_token| async move {
            let request = ListProjectsRequest { page_token, ..ListProjectsRequest::default() };
            client.list_projects(&request).await.map(|page| (page.projects, page.next_page_token))
        })
        .await?;
        choose(projects, selector.project.as_deref(), "project", |project| [&project.id, &project.name])
    }

    async fn environment(&self, selector: &EnvironmentArgs) -> io::Result<(Project, Environment)> {
        let project = self.project(&selector.project).await?;
        let client = &self.client;
        let project_id = &project.id;
        let environments = all(|page_token| async move {
            let request = ListEnvironmentsRequest { project_id: project_id.clone(), page_token, page_size: 0 };
            client.list_environments(&request).await.map(|page| (page.environments, page.next_page_token))
        })
        .await?;
        let environment = choose(environments, Some(&selector.environment), "environment", |environment| {
            [&environment.id, &environment.name]
        })?;
        Ok((project, environment))
    }
}

/// A client for `target` without credentials, whose unary calls time out.
fn client(target: &Target) -> Client {
    Client::new(target.url()).with_timeout(REQUEST_TIMEOUT)
}

/// A failed call, pointing at `chunk auth login` when the token was refused.
fn api_error(error: chunk_management::Error) -> io::Error {
    if error.code() == Code::Unauthenticated {
        return io::Error::other(format!("{error}. Log in again with `chunk auth login`."));
    }
    io::Error::other(error)
}

/// Every item of a paginated list, given a call for the page after a page token.
async fn all<T, Page>(mut page: impl FnMut(String) -> Page) -> io::Result<Vec<T>>
where
    Page: Future<Output = Result<(Vec<T>, String), chunk_management::Error>>,
{
    let mut items = Vec::new();
    let mut token = String::new();
    loop {
        let (batch, next) = page(token).await.map_err(api_error)?;
        items.extend(batch);
        if next.is_empty() {
            return Ok(items);
        }
        token = next;
    }
}

/// The item whose ID or name is `selector`; without one, the only item. A name several items share is ambiguous.
fn choose<T>(items: Vec<T>, selector: Option<&str>, kind: &str, key: fn(&T) -> [&String; 2]) -> io::Result<T> {
    let names = || items.iter().map(|item| key(item)[1].as_str()).collect::<Vec<_>>().join(", ");
    let Some(selector) = selector else {
        return match items.len() {
            0 => {
                Err(io::Error::other(format!("There are no {kind}s yet; create one with `chunk {kind}s create NAME`.")))
            }
            1 => Ok(items.into_iter().next().expect("one item")),
            _ => Err(io::Error::other(format!("Choose a {kind} with --{kind}: {}.", names()))),
        };
    };
    let by_id = items.iter().position(|item| key(item)[0] == selector);
    let by_name: Vec<usize> = (0..items.len()).filter(|&index| key(&items[index])[1] == selector).collect();
    let index = match (by_id, by_name.as_slice()) {
        (Some(index), _) => index,
        (None, [index]) => *index,
        (None, []) if items.is_empty() => {
            return Err(io::Error::other(format!("No {kind} is named {selector}; there are none.")));
        }
        (None, []) => return Err(io::Error::other(format!("No {kind} is named {selector}; there are: {}.", names()))),
        (None, _) => {
            let ids = by_name.iter().map(|&index| key(&items[index])[0].as_str()).collect::<Vec<_>>().join(", ");
            return Err(io::Error::other(format!("Several {kind}s are named {selector}; choose one by ID: {ids}.")));
        }
    };
    Ok(items.into_iter().nth(index).expect("found item"))
}

#[cfg(test)]
mod tests;
