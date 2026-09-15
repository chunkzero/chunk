use super::super::super::{
    platform::{Platform, request},
    transport::invalid_data,
};
use chunk_contract::Command;
use chunk_proto::v1::{CommandScope, backend_commands_client::BackendCommandsClient};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};
use tonic::{Request, transport::Channel};

pub(super) fn client(platform: &Platform) -> BackendCommandsClient<Channel> {
    platform.commands.clone()
}
pub(super) fn authenticated<T>(platform: &Platform, body: T) -> io::Result<Request<T>> {
    let backend = &platform.target.backend;
    let token = backend.platform_token.as_ref().ok_or_else(|| invalid_data("commands require platform authority"))?;
    let mut request = request(body, token)?;
    request.metadata_mut().insert("x-chunk-environment", backend.environment.parse().map_err(invalid_data)?);
    request.metadata_mut().insert("x-chunk-deployment", backend.deployment.parse().map_err(invalid_data)?);
    Ok(request)
}
pub(super) async fn catalog(
    platform: &Platform,
    scope: &CommandScope,
    expected: &BTreeMap<String, Command>,
) -> io::Result<BTreeSet<String>> {
    let response =
        client(platform).catalog(authenticated(platform, scope.clone())?).await.map_err(io::Error::other)?.into_inner();
    let descriptors: BTreeMap<String, Command> =
        serde_json::from_slice(&response.commands_json).map_err(invalid_data)?;
    if &descriptors != expected || response.allowed_ids.iter().any(|id| !descriptors.contains_key(id)) {
        return Err(invalid_data("command catalog differs from pinned manifest"));
    }
    Ok(response.allowed_ids.into_iter().collect())
}
