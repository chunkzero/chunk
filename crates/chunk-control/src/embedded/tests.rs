use super::*;
#[cfg(unix)]
use chunk_proto::v1::{
    ProcessIdentity, ProcessInventory,
    process_control_server::{ProcessControl, ProcessControlServer},
};
use std::path::Path;
#[cfg(unix)]
use std::time::Duration;
#[cfg(unix)]
use tonic::{Request, Response};

fn host(directory: &Path, java: std::path::PathBuf) -> EmbeddedHost {
    EmbeddedHost::new(
        chunk_runtime::server::Config {
            distribution: directory.into(),
            java,
            connection: directory.join("runtime.json"),
            deployment: chunk_runtime::DeploymentRef {
                environment: "local".into(),
                deployment: "test".into(),
            },
            machine_profile: "test".into(),
            artifact_digest: "test".into(),
            memory_mib: 512,
            backend: None,
        },
        BTreeMap::from([(
            "test".into(),
            MachineProfile {
                memory_mib: 512,
                max_sessions: 1,
            },
        )]),
    )
}

#[tokio::test]
async fn failed_launch_is_retired_and_shutdown_is_idempotent() {
    let directory = tempfile::tempdir().unwrap();
    let host = host(directory.path(), directory.path().join("missing-java"));
    let id = uuid::Uuid::new_v4().to_string();
    assert!(host.ensure(&id, "test").await.is_err());
    assert!(host.stopped(&id));
    assert!(matches!(host.ensure(&id, "test").await, Err(Error::Stopped)));
    host.terminate(&id).await.unwrap();
    host.shutdown().await.unwrap();
}

#[cfg(unix)]
struct FakeJvm;

#[cfg(unix)]
#[tonic::async_trait]
impl ProcessControl for FakeJvm {
    async fn create_session(
        &self,
        _: Request<chunk_proto::v1::SessionCommand>,
    ) -> std::result::Result<Response<chunk_proto::v1::SessionInventory>, tonic::Status> {
        Err(tonic::Status::unimplemented("fixture"))
    }

    async fn finish_session(
        &self,
        _: Request<chunk_proto::v1::SessionCommand>,
    ) -> std::result::Result<Response<chunk_proto::v1::SessionInventory>, tonic::Status> {
        Err(tonic::Status::unimplemented("fixture"))
    }

    async fn inventory(
        &self,
        request: Request<ProcessIdentity>,
    ) -> std::result::Result<Response<ProcessInventory>, tonic::Status> {
        Ok(Response::new(ProcessInventory {
            identity: Some(request.into_inner()),
            tick_count: 1,
            ..Default::default()
        }))
    }

    async fn stop_process(
        &self,
        request: Request<ProcessIdentity>,
    ) -> std::result::Result<Response<ProcessIdentity>, tonic::Status> {
        Ok(Response::new(request.into_inner()))
    }
}

#[cfg(unix)]
#[tokio::test]
async fn spontaneous_exit_retires_ready_runtime_without_termination() {
    use chunk_proto::v1::{ConfigurationResponse, ProcessRegistration, supervisor_client::SupervisorClient};
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("fake-java");
    std::fs::write(
        &program,
        r#"#!/bin/sh
printf '%s\n' "$CHUNK_SUPERVISOR" "$CHUNK_PROCESS_TOKEN" "$CHUNK_RUNTIME_ID" "$CHUNK_PROCESS_ID" > "$0.env"
while [ ! -f "$0.exit" ]; do sleep 0.05; done
exit 7
"#,
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let host = host(directory.path(), program.clone());
    let id = uuid::Uuid::new_v4().to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap().to_string();
    let stop = CancellationToken::new();
    let cancellation = stop.clone();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(ProcessControlServer::new(FakeJvm))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                cancellation.cancelled_owned(),
            )
            .await
            .unwrap();
    });
    let register = async {
        let fields = loop {
            if let Ok(environment) = std::fs::read_to_string(program.with_extension("env")) {
                let fields: Vec<String> = environment.lines().map(str::to_owned).collect();
                if fields.len() == 4 {
                    break fields;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let identity = ProcessIdentity {
            deployment: Some(host.config.deployment.clone()),
            runtime_id: fields[2].clone(),
            process_id: fields[3].clone(),
            generation: 1,
            machine_profile: "test".into(),
            artifact_digest: "test".into(),
        };
        let mut request = Request::new(ProcessRegistration {
            identity: Some(identity.clone()),
            control_endpoint: endpoint.clone(),
            player_endpoint: endpoint,
            configuration: Some(ConfigurationResponse {
                deployment: identity.deployment,
                process_generation: 1,
                runtime_id: identity.runtime_id,
                protocol: 775,
            }),
        });
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {}", fields[1]).parse().unwrap());
        SupervisorClient::connect(fields[0].clone())
            .await
            .unwrap()
            .register_process(request)
            .await
            .unwrap();
    };
    let (connection, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(host.ensure(&id, "test"), register)
    })
    .await
    .unwrap();
    connection.unwrap();
    assert!(!host.stopped(&id));
    std::fs::write(program.with_extension("exit"), b"exit").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !host.stopped(&id) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(host.ensure(&id, "test").await, Err(Error::Stopped)));
    host.shutdown().await.unwrap();
    stop.cancel();
    server.await.unwrap();
}
