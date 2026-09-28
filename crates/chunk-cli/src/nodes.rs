use chunk_proto::sync::v1::{DrainArguments, DrainResult, Node, drain_arguments::Target};
use serde_json::{Value, json};
use std::{io, path::PathBuf};

use crate::core::Core;

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
        #[arg(long, value_parser = crate::core::parse_operation)]
        operation: Option<uuid::Uuid>,
        /// Zero requests immediate shutdown.
        #[arg(long, default_value = "60")]
        timeout_seconds: u32,
    },
}
pub(crate) async fn run(options: Options) -> io::Result<()> {
    let core = Core::open(&options.control_file).await?;
    let output = match options.action {
        Action::List => {
            let nodes = core.snapshot::<Node>("nodes").await?;
            json!({"nodes": nodes.iter().map(|(host, node)| node_json(host, node)).collect::<Vec<_>>()})
        }
        Action::Shutdown { host, operation, timeout_seconds } => {
            let operation_id = crate::core::operation(operation);
            eprintln!("Node shutdown operation: {operation_id}");
            let arguments = DrainArguments { target: Some(Target::Host(host.to_string())), timeout_seconds };
            let drained: DrainResult = core.call("drain", &operation_id, &arguments).await?;
            let nodes = core.snapshot::<Node>("nodes").await?;
            let node = nodes.get(&drained.host).ok_or_else(|| io::Error::other("unknown node"))?;
            node_json(&drained.host, node)
        }
    };
    serde_json::to_writer_pretty(io::stdout().lock(), &output).map_err(io::Error::other)?;
    println!();
    Ok(())
}

/// `node` as `chunk nodes` prints it.
fn node_json(host: &str, node: &Node) -> Value {
    let health = node.health.as_ref().map(|health| {
        json!({
            "ready": health.ready,
            "draining": health.draining,
            "tick_count": health.tick_count,
            "last_tick_age_millis": health.last_tick_age_millis,
            "heap_used_bytes": health.heap_used_bytes,
            "heap_max_bytes": health.heap_max_bytes,
            "gc_count": health.gc_count,
            "gc_time_millis": health.gc_time_millis,
            "process_cpu_load": health.process_cpu_load,
            "sessions": health.sessions,
            "players": health.players,
        })
    });
    json!({
        "host_id": host,
        "app_id": node.app,
        "machine_profile": node.machine_profile,
        "phase": node.phase().as_str_name(),
        "health": health,
        "observed_at_ms": node.observed_at_ms,
        "consecutive_failures": node.consecutive_failures,
        "deployment": node.deployment,
    })
}

#[cfg(test)]
mod tests {
    use chunk_proto::sync::v1::{JvmHealth, NodePhase};

    use super::*;

    #[test]
    fn nodes_print_with_the_keys_of_earlier_releases() {
        let node = Node {
            deployment: "d87ec655-1".into(),
            app: "lobby".into(),
            machine_profile: "local".into(),
            phase: NodePhase::Online.into(),
            health: Some(JvmHealth { ready: true, players: 2, ..JvmHealth::default() }),
            observed_at_ms: 1_790_000_000_000,
            ..Node::default()
        };
        let printed = node_json("5a9e4aba-0000-4000-8000-000000000000", &node);
        let keys =
            |value: &Value| value.as_object().unwrap().keys().cloned().collect::<std::collections::BTreeSet<_>>();
        let expected = [
            "app_id",
            "consecutive_failures",
            "deployment",
            "health",
            "host_id",
            "machine_profile",
            "observed_at_ms",
            "phase",
        ];
        assert_eq!(keys(&printed), expected.map(String::from).into());
        assert_eq!(keys(&printed["health"]).len(), 11);
        assert_eq!((printed["app_id"].as_str(), printed["phase"].as_str()), (Some("lobby"), Some("NODE_PHASE_ONLINE")));
        assert!(node_json("host", &Node::default())["health"].is_null());
    }
}
