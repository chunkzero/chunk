//! Projects, environments, deployments, apps and logs, as the platform reports them.

use std::{
    fmt::Write as _,
    io::{self, IsTerminal, Write as _},
    time::Duration,
};

use chunk_management::{
    Client, Code,
    v1::{
        CreateEnvironmentRequest, CreateProjectRequest, DeleteEnvironmentRequest, Environment, ForkEnvironmentRequest,
        GetEnvironmentRequest, ListAppsRequest, ListDeploymentsRequest, ListEnvironmentsRequest, ListProjectsRequest,
        ListSnapshotsRequest, LogEntry, ReadLogsRequest,
    },
};
use clap::{Args, Subcommand};

use super::{EnvironmentArgs, ProjectArg, Session, all, api_error};

const DELETE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const DELETE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Args)]
pub(crate) struct Projects {
    #[command(subcommand)]
    action: Option<Create>,
}

#[derive(Args)]
pub(crate) struct Environments {
    #[command(flatten)]
    project: ProjectArg,
    #[command(subcommand)]
    action: Option<EnvironmentAction>,
}

#[derive(Subcommand)]
enum EnvironmentAction {
    /// Create one.
    Create {
        /// 1 to 63 lowercase letters, digits and hyphens, starting and ending with a letter or digit.
        name: String,
    },
    /// Delete one, destroying its machines and data.
    Delete {
        /// The environment's name or ID.
        environment: String,
        /// Wait until the environment is gone; deleting otherwise finishes in the background.
        #[arg(long)]
        wait: bool,
        /// Delete without asking, as a terminal-less run must.
        #[arg(long, short)]
        yes: bool,
    },
    /// Create one from a snapshot of another, running the source's active release. The source is untouched.
    Fork {
        /// The fork's name, following the same rules as create.
        name: String,
        /// The source environment's name or ID.
        #[arg(long)]
        from: String,
        /// One of the source's snapshots, as `snapshots` lists them; defaults to the source's latest state.
        #[arg(long)]
        snapshot: Option<String>,
        /// Copy the source's secrets; otherwise the fork starts with none.
        #[arg(long)]
        copy_secrets: bool,
    },
    /// List the snapshots stored in an environment's log, newest first.
    Snapshots {
        /// The environment's name or ID.
        environment: String,
    },
}

#[derive(Subcommand)]
enum Create {
    /// Create one.
    Create {
        /// 1 to 63 lowercase letters, digits and hyphens, starting and ending with a letter or digit.
        name: String,
    },
}

#[derive(Args)]
pub(crate) struct Deployments {
    #[command(flatten)]
    environment: EnvironmentArgs,
    /// How many to list, up to 200.
    #[arg(long, default_value = "20", value_parser = clap::value_parser!(u8).range(1..=200))]
    limit: u8,
}

#[derive(Args)]
pub(crate) struct Apps {
    #[command(flatten)]
    environment: EnvironmentArgs,
}

#[derive(Args)]
pub(crate) struct Logs {
    #[command(flatten)]
    environment: EnvironmentArgs,
    /// Keep printing new entries as they arrive.
    #[arg(long, short)]
    follow: bool,
    /// Only this app's JVM entries.
    #[arg(long)]
    app: Option<String>,
    /// How many recent entries to print first; the platform caps it.
    #[arg(long, default_value = "200")]
    limit: u32,
}

pub(super) async fn projects(options: Projects) -> io::Result<()> {
    let session = Session::open()?;
    let client = &session.client;
    if let Some(Create::Create { name }) = options.action {
        let request = CreateProjectRequest { request_id: request_id(), name, owner_id: String::new() };
        let project = client.create_project(&request).await.map_err(api_error)?.project.unwrap_or_default();
        return cliclack::log::success(format!("Created project {} ({})", project.name, project.id));
    }
    let projects = all(|page_token| async move {
        let request = ListProjectsRequest { page_token, ..ListProjectsRequest::default() };
        client.list_projects(&request).await.map(|page| (page.projects, page.next_page_token))
    })
    .await?;
    table(
        ["NAME", "ID", "CREATED"],
        projects.into_iter().map(|project| [project.name, project.id, time(project.create_time)]),
    )
}

pub(super) async fn environments(options: Environments) -> io::Result<()> {
    let session = Session::open()?;
    let project = session.project(&options.project).await?;
    let client = &session.client;
    match options.action {
        Some(EnvironmentAction::Create { name }) => {
            let request = CreateEnvironmentRequest { request_id: request_id(), project_id: project.id, name };
            let environment =
                client.create_environment(&request).await.map_err(api_error)?.environment.unwrap_or_default();
            return cliclack::log::success(format!(
                "Created environment {} ({}) in {}",
                environment.name, environment.id, project.name
            ));
        }
        Some(EnvironmentAction::Delete { environment, wait, yes }) => {
            let environment = session.environment_in(&project, &environment).await?;
            return delete_environment(client, &environment, wait, yes).await;
        }
        Some(EnvironmentAction::Fork { name, from, snapshot, copy_secrets }) => {
            let source = session.environment_in(&project, &from).await?;
            let request = ForkEnvironmentRequest {
                request_id: request_id(),
                source_environment_id: source.id.clone(),
                snapshot_id: snapshot.unwrap_or_default(),
                name,
                copy_secrets,
            };
            let environment =
                client.fork_environment(&request).await.map_err(api_error)?.environment.unwrap_or_default();
            return cliclack::log::success(format!(
                "Forked environment {} ({}) from {}; it starts with the source's active release",
                environment.name, environment.id, source.name
            ));
        }
        Some(EnvironmentAction::Snapshots { environment }) => {
            let environment = session.environment_in(&project, &environment).await?;
            let environment_id = &environment.id;
            let snapshots = all(|page_token| async move {
                let request = ListSnapshotsRequest { environment_id: environment_id.clone(), page_token, page_size: 0 };
                client.list_snapshots(&request).await.map(|page| (page.snapshots, page.next_page_token))
            })
            .await?;
            return table(
                ["ID", "CREATED"],
                snapshots.into_iter().map(|snapshot| [snapshot.id, time(snapshot.create_time)]),
            );
        }
        None => {}
    }
    let project_id = &project.id;
    let environments = all(|page_token| async move {
        let request = ListEnvironmentsRequest { project_id: project_id.clone(), page_token, page_size: 0 };
        client.list_environments(&request).await.map(|page| (page.environments, page.next_page_token))
    })
    .await?;
    table(
        ["NAME", "ID", "STATE", "PLAYERS", "JOIN ADDRESS", "ACTIVE DEPLOYMENT"],
        environments.into_iter().map(|environment| {
            let state = label(environment.state().as_str_name(), "ENVIRONMENT_STATE_");
            let players = environment.online_players.to_string();
            [
                environment.name,
                environment.id,
                state,
                players,
                environment.join_address,
                environment.active_deployment_id,
            ]
        }),
    )
}

/// Deletes the environment after confirming, and with `wait` follows its removal until management reports it gone.
async fn delete_environment(client: &Client, environment: &Environment, wait: bool, yes: bool) -> io::Result<()> {
    let name = &environment.name;
    if !yes {
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return Err(io::Error::other("Use --yes to delete an environment without a terminal."));
        }
        let confirmed = cliclack::confirm(format!("Delete environment {name}? Its machines and data are destroyed."))
            .initial_value(false)
            .interact()?;
        if !confirmed {
            return cliclack::log::info("Kept the environment");
        }
    }
    let request = DeleteEnvironmentRequest { environment_id: environment.id.clone() };
    client.delete_environment(&request).await.map_err(api_error)?;
    if !wait {
        return cliclack::log::success(format!("Deleting environment {name}; it finishes in the background"));
    }
    cliclack::log::info(format!("Deleting environment {name}…"))?;
    chunk_service::run(|stop| async move {
        tokio::select! {
            gone = until_gone(client, &environment.id) => gone?,
            () = stop.cancelled() => {
                cliclack::log::info(format!("Stopped waiting; environment {name} is still being deleted."))?;
                return Err(io::Error::new(io::ErrorKind::Interrupted, "stopped waiting"));
            }
        }
        cliclack::log::success(format!("Deleted environment {name}"))
    })
    .await
}

/// Polls until `GetEnvironment` reports the environment not found, or the wait times out.
async fn until_gone(client: &Client, environment_id: &str) -> io::Result<()> {
    let request = GetEnvironmentRequest { environment_id: environment_id.into() };
    let poll = async {
        loop {
            match client.get_environment(&request).await {
                Err(error) if error.code() == Code::NotFound => return Ok(()),
                // A restarting or slow platform answers again shortly; the deletion goes on without it.
                Ok(_) => {}
                Err(error) if error.code() == Code::Unavailable => {}
                Err(error) => return Err(api_error(error)),
            }
            tokio::time::sleep(DELETE_POLL_INTERVAL).await;
        }
    };
    tokio::time::timeout(DELETE_TIMEOUT, poll)
        .await
        .map_err(|_| io::Error::other("Timed out waiting for the environment to go; the deletion continues."))?
}

pub(super) async fn deployments(options: Deployments) -> io::Result<()> {
    let session = Session::open()?;
    let (_, environment) = session.environment(&options.environment).await?;
    let request = ListDeploymentsRequest {
        environment_id: environment.id,
        page_size: options.limit.into(),
        page_token: String::new(),
    };
    let deployments = session.client.list_deployments(&request).await.map_err(api_error)?.deployments;
    table(
        ["ID", "STATE", "TRIGGER", "RELEASE", "CREATED", "MESSAGE"],
        deployments.into_iter().map(|deployment| {
            let state = label(deployment.state().as_str_name(), "DEPLOYMENT_STATE_");
            let trigger = label(deployment.trigger().as_str_name(), "DEPLOYMENT_TRIGGER_");
            [deployment.id, state, trigger, deployment.release_id, time(deployment.create_time), deployment.message]
        }),
    )
}

pub(super) async fn apps(options: Apps) -> io::Result<()> {
    let session = Session::open()?;
    let (_, environment) = session.environment(&options.environment).await?;
    let request = ListAppsRequest { environment_id: environment.id, release_id: String::new() };
    let apps = session.client.list_apps(&request).await.map_err(api_error)?.apps;
    table(["APP", "SESSION TYPES"], apps.into_iter().map(|app| [app.id, app.sessions.join(", ")]))
}

pub(super) async fn logs(options: Logs) -> io::Result<()> {
    let session = Session::open()?;
    let (_, environment) = session.environment(&options.environment).await?;
    let request = ReadLogsRequest {
        environment_id: environment.id,
        app_id: options.app.unwrap_or_default(),
        limit: options.limit,
        follow: options.follow,
        ..ReadLogsRequest::default()
    };
    let mut stream = session.client.read_logs(&request).await.map_err(api_error)?;
    let stdout = io::stdout();
    while let Some(batch) = stream.message().await.map_err(api_error)? {
        let mut output = stdout.lock();
        for entry in &batch.entries {
            writeln!(output, "{}", log_line(entry))?;
        }
        output.flush()?;
    }
    Ok(())
}

pub(super) fn log_line(entry: &LogEntry) -> String {
    let severity = label(entry.severity().as_str_name(), "LOG_SEVERITY_").to_ascii_uppercase();
    let mut source = label(entry.source().as_str_name(), "LOG_SOURCE_");
    if !entry.app_id.is_empty() {
        source = format!("{source}/{}", entry.app_id);
    }
    let time = entry.time.map(|time| time.to_string()).unwrap_or_default();
    format!("{time} {severity:<5} {source} {}", entry.message)
}

/// An enum value's name without its prefix, in lowercase words: `DEPLOYMENT_STATE_IN_PROGRESS` is `in progress`.
pub(super) fn label(name: &str, prefix: &str) -> String {
    name.strip_prefix(prefix).unwrap_or(name).to_ascii_lowercase().replace('_', " ")
}

pub(super) fn time(time: Option<prost_types::Timestamp>) -> String {
    time.map(|time| prost_types::Timestamp { nanos: 0, ..time }.to_string()).unwrap_or_default()
}

pub(super) fn request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Prints rows under a header, in columns as wide as their widest cell.
pub(super) fn table<const N: usize>(header: [&str; N], rows: impl IntoIterator<Item = [String; N]>) -> io::Result<()> {
    let rows: Vec<[String; N]> = std::iter::once(header.map(String::from)).chain(rows).collect();
    let widths: [usize; N] =
        std::array::from_fn(|column| rows.iter().map(|row| row[column].chars().count()).max().unwrap_or(0));
    let mut output = io::stdout().lock();
    for row in &rows {
        let mut line = String::new();
        for (cell, width) in row.iter().zip(widths) {
            write!(line, "{cell:width$}  ").expect("writing to a string");
        }
        writeln!(output, "{}", line.trim_end())?;
    }
    Ok(())
}
