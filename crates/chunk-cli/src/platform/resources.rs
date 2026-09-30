//! Projects, environments, deployments, apps and logs, as the platform reports them.

use std::{
    fmt::Write as _,
    io::{self, Write as _},
};

use chunk_management::v1::{
    CreateEnvironmentRequest, CreateProjectRequest, ListAppsRequest, ListDeploymentsRequest, ListEnvironmentsRequest,
    ListProjectsRequest, LogEntry, ReadLogsRequest,
};
use clap::{Args, Subcommand};

use super::{EnvironmentArgs, ProjectArg, Session, all, api_error};

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
    action: Option<Create>,
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
    /// How many to list.
    #[arg(long, default_value = "20")]
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
    if let Some(Create::Create { name }) = options.action {
        let request = CreateEnvironmentRequest { request_id: request_id(), project_id: project.id, name };
        let environment = client.create_environment(&request).await.map_err(api_error)?.environment.unwrap_or_default();
        return cliclack::log::success(format!(
            "Created environment {} ({}) in {}",
            environment.name, environment.id, project.name
        ));
    }
    let project_id = &project.id;
    let environments = all(|page_token| async move {
        let request = ListEnvironmentsRequest { project_id: project_id.clone(), page_token, page_size: 0 };
        client.list_environments(&request).await.map(|page| (page.environments, page.next_page_token))
    })
    .await?;
    table(
        ["NAME", "ID", "STATE", "PLAYERS", "HOSTNAME", "ACTIVE DEPLOYMENT"],
        environments.into_iter().map(|environment| {
            let state = label(environment.state().as_str_name(), "ENVIRONMENT_STATE_");
            let players = environment.online_players.to_string();
            [environment.name, environment.id, state, players, environment.hostname, environment.active_deployment_id]
        }),
    )
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

fn time(time: Option<prost_types::Timestamp>) -> String {
    time.map(|time| prost_types::Timestamp { nanos: 0, ..time }.to_string()).unwrap_or_default()
}

fn request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Prints rows under a header, in columns as wide as their widest cell.
fn table<const N: usize>(header: [&str; N], rows: impl IntoIterator<Item = [String; N]>) -> io::Result<()> {
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
