//! The operator's client of core's sync protocol, presenting control's credential from `control.json`.

use std::{collections::BTreeMap, io, path::Path, time::Duration};

use chunk_contract::ControlConnection;
use chunk_proto::sync::v1::{
    CallRequest, SubscribeRequest, Update, call_response::Outcome, core_client::CoreClient, entry::State,
};
use prost::Message;
use tonic::{Request, Streaming, transport::Channel};

pub(crate) const TIMEOUT: Duration = Duration::from_secs(5);
/// Operator methods take operation IDs with this prefix.
const OPERATION_PREFIX: &str = "operator:";
/// Core sends messages of up to 16 MiB.
const MESSAGE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct Core {
    client: CoreClient<Channel>,
    token: String,
}

impl Core {
    /// Connects to the core the connection record at `path` names.
    pub async fn open(path: &Path) -> io::Result<Self> {
        let connection: ControlConnection = serde_json::from_slice(&std::fs::read(path)?).map_err(io::Error::other)?;
        Self::connect(&connection).await
    }

    /// Connects to core at `connection`'s endpoint, which must be loopback HTTP.
    pub async fn connect(connection: &ControlConnection) -> io::Result<Self> {
        let address: std::net::SocketAddr = connection
            .endpoint
            .strip_prefix("http://")
            .ok_or_else(|| io::Error::other("core requires loopback HTTP"))?
            .parse()
            .map_err(io::Error::other)?;
        if !address.ip().is_loopback() {
            return Err(io::Error::other("core requires loopback HTTP"));
        }
        let channel = Channel::from_shared(connection.endpoint.clone())
            .map_err(io::Error::other)?
            .connect_timeout(Duration::from_secs(3))
            .connect()
            .await
            .map_err(io::Error::other)?;
        let client = CoreClient::new(channel).max_decoding_message_size(MESSAGE_BYTES);
        Ok(Self { client, token: connection.token.clone() })
    }

    /// Calls platform method `chunk:<method>` under `operation`, returning its result.
    pub async fn call<R: Message + Default>(
        &self,
        method: &str,
        operation: &str,
        arguments: &impl Message,
    ) -> io::Result<R> {
        let mut request = self.request(CallRequest {
            operation_id: operation.to_owned(),
            method: format!("chunk:{method}"),
            arguments: arguments.encode_to_vec(),
            ..CallRequest::default()
        })?;
        request.set_timeout(TIMEOUT);
        let response = self.client.clone().call(request).await.map_err(io::Error::other)?.into_inner();
        match response.outcome {
            Some(Outcome::Result(result)) => R::decode(result.as_slice()).map_err(io::Error::other),
            Some(Outcome::Error(error)) => Err(io::Error::other(error.message)),
            None => Err(io::Error::other("core returned no outcome")),
        }
    }

    /// Follows `topic` from a snapshot.
    pub async fn follow<T: Message + Default>(&self, topic: &str) -> io::Result<Topic<T>> {
        let request = self.request(SubscribeRequest { topic: topic.to_owned(), ..SubscribeRequest::default() })?;
        let updates = self.client.clone().subscribe(request).await.map_err(io::Error::other)?.into_inner();
        Ok(Topic { updates, view: View::default() })
    }

    /// `topic`'s entries at its first snapshot.
    pub async fn snapshot<T: Message + Default>(&self, topic: &str) -> io::Result<BTreeMap<String, T>> {
        let snapshot = async {
            let mut topic = self.follow(topic).await?;
            topic.next().await?;
            Ok(topic.view.entries)
        };
        tokio::time::timeout(TIMEOUT, snapshot).await.map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
    }

    fn request<T>(&self, body: T) -> io::Result<Request<T>> {
        let mut request = Request::new(body);
        let credential = format!("Bearer {}", self.token).parse().map_err(io::Error::other)?;
        request.metadata_mut().insert("authorization", credential);
        Ok(request)
    }
}

/// An operator method's operation ID: the prefix and `id`, or a new UUID.
pub(crate) fn operation(id: Option<uuid::Uuid>) -> String {
    format!("{OPERATION_PREFIX}{}", id.unwrap_or_else(uuid::Uuid::new_v4))
}

/// Parses `--operation`: a UUID, with or without the operator prefix.
pub(crate) fn parse_operation(value: &str) -> Result<uuid::Uuid, uuid::Error> {
    value.strip_prefix(OPERATION_PREFIX).unwrap_or(value).parse()
}

/// One subscription to a topic, and the view its updates built.
pub(crate) struct Topic<T> {
    updates: Streaming<Update>,
    view: View<T>,
}

impl<T: Message + Default> Topic<T> {
    /// Waits for the next whole update and returns the view after it.
    pub async fn next(&mut self) -> io::Result<&BTreeMap<String, T>> {
        loop {
            let update = self.updates.message().await.map_err(io::Error::other)?;
            let update = update.ok_or_else(|| io::Error::other("core ended the subscription"))?;
            if self.view.apply(update)? {
                return Ok(&self.view.entries);
            }
        }
    }
}

/// A topic's entries by key, applying each run of `continued` updates together.
struct View<T> {
    entries: BTreeMap<String, T>,
    pending: Vec<Update>,
}

impl<T> Default for View<T> {
    fn default() -> Self {
        Self { entries: BTreeMap::new(), pending: Vec::new() }
    }
}

impl<T: Message + Default> View<T> {
    /// Applies `update`, or holds it until the run it continues ends; true once the entries changed. Fails on the
    /// stream's final error.
    fn apply(&mut self, update: Update) -> io::Result<bool> {
        if let Some(error) = update.error {
            return Err(io::Error::other(error.message));
        }
        let continued = update.continued;
        self.pending.push(update);
        if continued {
            return Ok(false);
        }
        for update in std::mem::take(&mut self.pending) {
            if update.snapshot {
                self.entries.clear();
            }
            for key in update.removed {
                self.entries.remove(&key);
            }
            for entry in update.upserts {
                match entry.state {
                    Some(State::Value(value)) => {
                        self.entries.insert(entry.key, T::decode(value.as_slice()).map_err(io::Error::other)?);
                    }
                    _ => {
                        self.entries.remove(&entry.key);
                    }
                }
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use chunk_proto::sync::v1::{Entry, Node};

    use super::*;

    fn update(snapshot: bool, continued: bool, hosts: &[&str], removed: &[&str]) -> Update {
        let upserts = hosts
            .iter()
            .map(|host| Entry { key: (*host).into(), state: Some(State::Value(Node::default().encode_to_vec())) })
            .collect();
        let removed = removed.iter().map(|&host| host.into()).collect();
        Update { snapshot, continued, upserts, removed, ..Update::default() }
    }

    #[test]
    fn continued_updates_apply_together_and_a_snapshot_replaces_the_view() {
        let mut view = View::<Node>::default();
        assert!(!view.apply(update(true, true, &["a"], &[])).unwrap());
        assert!(view.entries.is_empty());
        assert!(view.apply(update(false, false, &["b"], &[])).unwrap());
        assert_eq!(view.entries.keys().collect::<Vec<_>>(), ["a", "b"]);
        assert!(view.apply(update(false, false, &["c"], &["a"])).unwrap());
        assert_eq!(view.entries.keys().collect::<Vec<_>>(), ["b", "c"]);
        assert!(view.apply(update(true, false, &["d"], &[])).unwrap());
        assert_eq!(view.entries.keys().collect::<Vec<_>>(), ["d"]);
    }
}
