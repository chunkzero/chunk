//! The calls the environment, edges and the CLI make. Mutations that take a `request_id` leave choosing it to the
//! caller, so a retry can reuse it.

use crate::{Client, Error, Stream, v1};

macro_rules! unary {
    ($($(#[$doc:meta])* $name:ident: $service:literal / $method:literal ($request:ident) -> $response:ident;)*) => {
        impl Client {
            $(
                $(#[$doc])*
                ///
                /// # Errors
                /// The service's status, or a transport or protocol failure.
                pub async fn $name(&self, request: &v1::$request) -> Result<v1::$response, Error> {
                    self.unary(concat!("chunk.management.v1.", $service, "/", $method), request).await
                }
            )*
        }
    };
}

macro_rules! server_stream {
    ($($(#[$doc:meta])* $name:ident: $service:literal / $method:literal ($request:ident) -> $response:ident;)*) => {
        impl Client {
            $(
                $(#[$doc])*
                ///
                /// # Errors
                /// The service's status before the stream starts, or a transport failure.
                pub async fn $name(&self, request: &v1::$request) -> Result<Stream<v1::$response>, Error> {
                    self.server_stream(concat!("chunk.management.v1.", $service, "/", $method), request).await
                }
            )*
        }
    };
}

unary! {
    /// Starts a device login; the CLI shows its URL and polls.
    start_login: "AuthService" / "StartLogin" (StartLoginRequest) -> StartLoginResponse;
    /// Polls a device login until it is approved, denied or expired.
    poll_login: "AuthService" / "PollLogin" (PollLoginRequest) -> PollLoginResponse;
    get_current_principal:
        "AuthService" / "GetCurrentPrincipal" (GetCurrentPrincipalRequest) -> GetCurrentPrincipalResponse;
    revoke_token: "AuthService" / "RevokeToken" (RevokeTokenRequest) -> RevokeTokenResponse;

    create_project: "ProjectService" / "CreateProject" (CreateProjectRequest) -> CreateProjectResponse;
    list_projects: "ProjectService" / "ListProjects" (ListProjectsRequest) -> ListProjectsResponse;
    create_environment:
        "ProjectService" / "CreateEnvironment" (CreateEnvironmentRequest) -> CreateEnvironmentResponse;
    get_environment: "ProjectService" / "GetEnvironment" (GetEnvironmentRequest) -> GetEnvironmentResponse;
    list_environments: "ProjectService" / "ListEnvironments" (ListEnvironmentsRequest) -> ListEnvironmentsResponse;
    /// Starts deleting an environment, which finishes in the background; `get_environment` is not found once it has.
    delete_environment:
        "ProjectService" / "DeleteEnvironment" (DeleteEnvironmentRequest) -> DeleteEnvironmentResponse;

    upload_release: "DeploymentService" / "UploadRelease" (UploadReleaseRequest) -> UploadReleaseResponse;
    complete_release_upload:
        "DeploymentService" / "CompleteReleaseUpload" (CompleteReleaseUploadRequest) -> CompleteReleaseUploadResponse;
    deploy: "DeploymentService" / "Deploy" (DeployRequest) -> DeployResponse;
    promote: "DeploymentService" / "Promote" (PromoteRequest) -> PromoteResponse;
    rollback: "DeploymentService" / "Rollback" (RollbackRequest) -> RollbackResponse;
    get_deployment: "DeploymentService" / "GetDeployment" (GetDeploymentRequest) -> GetDeploymentResponse;
    list_deployments: "DeploymentService" / "ListDeployments" (ListDeploymentsRequest) -> ListDeploymentsResponse;
    list_apps: "DeploymentService" / "ListApps" (ListAppsRequest) -> ListAppsResponse;

    /// Core's status report, also its heartbeat; fenced by the lease from `attach`.
    report_status: "EnvironmentService" / "ReportStatus" (ReportStatusRequest) -> ReportStatusResponse;
    set_wake_alarm: "EnvironmentService" / "SetWakeAlarm" (SetWakeAlarmRequest) -> SetWakeAlarmResponse;
    ensure_capacity: "EnvironmentService" / "EnsureCapacity" (EnsureCapacityRequest) -> EnsureCapacityResponse;
    release_capacity: "EnvironmentService" / "ReleaseCapacity" (ReleaseCapacityRequest) -> ReleaseCapacityResponse;

    /// Reports client addresses that failed Minecraft authentication at a gateway.
    report_failed_auth:
        "EnvironmentService" / "ReportFailedAuth" (ReportFailedAuthRequest) -> ReportFailedAuthResponse;

    /// Asks for a sleeping environment to wake for a login or ping.
    wake: "EdgeService" / "Wake" (WakeRequest) -> WakeResponse;
}

server_stream! {
    /// Registers an environment process and streams its desired state, including its log store and restore.
    attach: "EnvironmentService" / "Attach" (AttachRequest) -> AttachResponse;
    /// Streams the routing table: every route first, then changes.
    watch_routes: "EdgeService" / "WatchRoutes" (WatchRoutesRequest) -> WatchRoutesResponse;
    /// Streams an environment's stored log entries, then new ones when following.
    read_logs: "LogService" / "ReadLogs" (ReadLogsRequest) -> ReadLogsResponse;
}
