use std::{io, path::PathBuf, time::Duration};

use chunk_proto::v1::{DrainRequest, MovePlayerRequest, SessionDemand, local_control_client::LocalControlClient};
use tonic::{Request, transport::Channel};

#[derive(clap::Args)]
pub(crate) struct Options {
    #[arg(long, default_value = ".chunk/control.json")]
    control_file: PathBuf,
    #[arg(long)]
    player: uuid::Uuid,
    /// Retain this ID when retrying an uncertain command.
    #[arg(long)]
    operation: Option<uuid::Uuid>,
    #[command(subcommand)]
    action: Action,
}

#[derive(clap::Subcommand)]
enum Action {
    /// Move this player on their existing public connection.
    Move {
        #[arg(long)]
        session_type: String,
        #[arg(long)]
        key: String,
        #[arg(long, default_value = "local")]
        machine_profile: String,
    },
    /// Retire the player's current runtime, moving its players before the deadline.
    Drain {
        #[arg(long, default_value = "60")]
        timeout_seconds: u32,
    },
}

pub(crate) async fn run(options: Options) -> io::Result<()> {
    let connection: chunk_contract::ControlConnection =
        serde_json::from_slice(&std::fs::read(options.control_file)?).map_err(io::Error::other)?;
    let address: std::net::SocketAddr = connection
        .endpoint
        .strip_prefix("http://")
        .ok_or_else(|| io::Error::other("control requires loopback HTTP"))?
        .parse()
        .map_err(io::Error::other)?;
    if !address.ip().is_loopback() {
        return Err(io::Error::other("control requires loopback HTTP"));
    }
    let channel = Channel::from_shared(connection.endpoint)
        .map_err(io::Error::other)?
        .connect_timeout(Duration::from_secs(3))
        .connect()
        .await
        .map_err(io::Error::other)?;
    let mut client = LocalControlClient::new(channel);
    let operation_id = options.operation.unwrap_or_else(uuid::Uuid::new_v4).to_string();
    cliclack::log::info(format!("Player operation: {operation_id}"))?;
    match options.action {
        Action::Move { session_type, key, machine_profile } => {
            client
                .move_player(auth(
                    MovePlayerRequest {
                        operation_id,
                        player_id: options.player.to_string(),
                        demand: Some(SessionDemand { session_type, key, machine_profile }),
                    },
                    &connection.token,
                )?)
                .await
                .map_err(io::Error::other)?;
            cliclack::log::success("Move queued.")?;
        }
        Action::Drain { timeout_seconds } => {
            let request = DrainRequest { operation_id, player_id: options.player.to_string(), timeout_seconds };
            let deadline = tokio::time::Instant::now() + Duration::from_secs(u64::from(timeout_seconds.min(120)) + 30);
            loop {
                let status = client
                    .drain(auth(request.clone(), &connection.token)?)
                    .await
                    .map_err(io::Error::other)?
                    .into_inner();
                if status.stopped {
                    cliclack::log::success("Runtime drained.")?;
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(io::Error::other(
                        "drain shutdown remains unresolved; retry with the same operation ID",
                    ));
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    Ok(())
}

fn auth<T>(body: T, token: &str) -> io::Result<Request<T>> {
    let mut request = Request::new(body);
    request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().map_err(io::Error::other)?);
    request.set_timeout(Duration::from_secs(5));
    Ok(request)
}
