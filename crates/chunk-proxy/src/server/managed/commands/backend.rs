use crate::server::{platform::Platform, transport::invalid_data};
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
    let token = platform
        .target
        .backend
        .platform_token
        .as_ref()
        .ok_or_else(|| invalid_data("commands require platform authority"))?;
    platform.backend_request(body, token)
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
