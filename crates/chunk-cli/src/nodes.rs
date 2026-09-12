use chunk_proto::v1::{NodePhase, NodeStatus, NodesRequest, ShutdownNodeRequest};
use std::{io, path::PathBuf};

#[derive(clap::Args)]
pub(crate) struct Options {
    #[arg(long, default_value = ".chunk/local/control.json")]
    control_file: PathBuf,
    #[command(subcommand)]
    action: Action,
}
#[derive(clap::Subcommand)]
enum Action {
    /// Show lifecycle state and last observed JVM metrics as JSON.
    List,
    /// Stop new placement, evacuate players, then terminate by the deadline.
    Shutdown {
        host: uuid::Uuid,
        /// Retain this ID when retrying an uncertain command.
        #[arg(long)]
        operation: Option<uuid::Uuid>,
        /// Zero requests immediate shutdown.
        #[arg(long, default_value = "60")]
        timeout_seconds: u32,
    },
}
pub(crate) async fn run(options: Options) -> io::Result<()> {
    let (mut client, token) = crate::players::connect(&options.control_file).await?;
    let output = match options.action {
        Action::List => {
            let nodes = client
                .nodes(crate::players::auth(NodesRequest {}, &token)?)
                .await
                .map_err(io::Error::other)?
                .into_inner()
                .nodes;
            let nodes = nodes.into_iter().map(status_json).collect::<io::Result<Vec<_>>>()?;
            Ok(serde_json::json!({"nodes": nodes}))
        }
        Action::Shutdown { host, operation, timeout_seconds } => {
            let operation_id = operation.unwrap_or_else(uuid::Uuid::new_v4).to_string();
            eprintln!("Node shutdown operation: {operation_id}");
            status_json(
                client
                    .shutdown_node(crate::players::auth(
                        ShutdownNodeRequest { operation_id, host_id: host.to_string(), timeout_seconds },
                        &token,
                    )?)
                    .await
                    .map_err(io::Error::other)?
                    .into_inner(),
            )
        }
    }?;
    serde_json::to_writer_pretty(io::stdout().lock(), &output).map_err(io::Error::other)?;
    println!();
    Ok(())
}

fn status_json(status: NodeStatus) -> io::Result<serde_json::Value> {
    let phase = NodePhase::try_from(status.phase).unwrap_or(NodePhase::Unspecified).as_str_name();
    let mut value = serde_json::to_value(status).map_err(io::Error::other)?;
    value["phase"] = phase.into();
    Ok(value)
}
