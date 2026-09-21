mod backend;
mod effects;
mod input;
mod run;
mod scope;
mod session;

use super::super::{
    platform::Platform,
    transport::{Transport, invalid_data},
};
use crate::command_tree::CommandTreeCatalog;
use chunk_contract::{Command, DomainManifest};
use chunk_proto::v1::{Assignment, ClaimRequest};
use chunk_protocol::{commands::CommandTree, encode_packet};
use scope::Origin;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::Arc,
};
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

pub(super) enum Output {
    Permissions {
        scope: String,
        result: io::Result<BTreeSet<String>>,
    },
    Packets {
        origin: Box<Origin>,
        invocation: CancellationToken,
        follow: bool,
        packets: Vec<Vec<u8>>,
        acknowledgment: Option<oneshot::Sender<io::Result<()>>>,
    },
}
#[derive(Clone)]
struct Tasks {
    platform: Platform,
    connection: CancellationToken,
    invocation: CancellationToken,
    current: watch::Receiver<Option<Origin>>,
    output: mpsc::Sender<Output>,
    capacity: Arc<Semaphore>,
    methods: Arc<Semaphore>,
    effects: Arc<Semaphore>,
}
impl Tasks {
    fn current(&self, origin: &Origin, follow: bool) -> io::Result<Origin> {
        if self.connection.is_cancelled() || (!follow && origin.cancellation.is_cancelled()) {
            return Err(invalid_data("command scope canceled"));
        }
        let current = self.current.borrow().clone().ok_or_else(|| invalid_data("player is configuring"))?;
        if (!follow && !current.matches(origin))
            || current.claim.connection_id != origin.claim.connection_id
            || current.claim.identity != origin.claim.identity
        {
            return Err(invalid_data("command player ownership changed"));
        }
        Ok(current)
    }
}

pub(super) struct Commands {
    tasks: Tasks,
    state: watch::Sender<Option<Origin>>,
    output: mpsc::Receiver<Output>,
    manifest: Option<Arc<DomainManifest>>,
    origin: Option<Origin>,
    catalog: Option<CommandTreeCatalog>,
    descriptors: BTreeMap<String, Command>,
    allowed: BTreeSet<String>,
    tree_received: bool,
    refreshing: bool,
}
impl Commands {
    pub async fn new(platform: &Platform) -> io::Result<Self> {
        let (output, receiver) = mpsc::channel(32);
        let (state, current) = watch::channel(None);
        Ok(Self {
            tasks: Tasks {
                platform: platform.clone(),
                connection: CancellationToken::new(),
                invocation: CancellationToken::new(),
                current,
                output,
                capacity: Arc::new(Semaphore::new(8)),
                methods: Arc::new(Semaphore::new(8)),
                effects: Arc::new(Semaphore::new(8)),
            },
            state,
            output: receiver,
            manifest: platform.manifest().await?,
            origin: None,
            catalog: None,
            descriptors: BTreeMap::new(),
            allowed: BTreeSet::new(),
            tree_received: false,
            refreshing: false,
        })
    }
    pub fn bind(&mut self, claim: &ClaimRequest, assignment: &Assignment) -> io::Result<()> {
        self.configuration();
        let Some(manifest) = &self.manifest else {
            self.origin = None;
            return Ok(());
        };
        let app = claim
            .demand
            .as_ref()
            .and_then(|d| d.session_type.split_once('/'))
            .map(|(app, _)| app)
            .ok_or_else(|| invalid_data("missing command destination"))?;
        let domain = manifest.apps.get(app).ok_or_else(|| invalid_data("missing command domain"))?.clone();
        self.descriptors = manifest
            .commands
            .iter()
            .filter(|(_, command)| {
                command.domain.is_empty()
                    || command.domain == domain
                    || domain.strip_prefix(&command.domain).is_some_and(|suffix| suffix.starts_with('/'))
            })
            .map(|(id, command)| (id.clone(), command.clone()))
            .collect();
        self.catalog =
            Some(CommandTreeCatalog::new(CommandTree::empty(), &self.descriptors, &domain).map_err(invalid_data)?);
        self.origin = Some(Origin::new(claim, assignment, domain)?);
        self.tree_received = false;
        self.allowed.clear();
        self.refreshing = false;
        Ok(())
    }
    pub fn arrived(&mut self) {
        self.state.send_replace(self.origin.clone());
        self.refresh();
    }
    pub fn configuration(&mut self) {
        if let Some(origin) = &self.origin {
            origin.cancellation.cancel();
        }
        self.state.send_replace(None);
    }
    pub fn tree(&mut self, jvm: &CommandTree) -> io::Result<Vec<u8>> {
        let Some(origin) = &self.origin else {
            return encode_packet(jvm).map_err(invalid_data);
        };
        let catalog = match CommandTreeCatalog::new(jvm.clone(), &self.descriptors, &origin.scope.domain) {
            Ok(catalog) => catalog,
            Err(error) => {
                let roots: Vec<_> = jvm.nodes[jvm.root]
                    .children
                    .iter()
                    .filter_map(|index| jvm.nodes[*index].name())
                    .filter(|name| {
                        self.descriptors.values().any(|command| {
                            std::iter::once(&command.name)
                                .chain(&command.aliases)
                                .any(|root| root.eq_ignore_ascii_case(name))
                        })
                    })
                    .collect();
                tracing::warn!(%error, ?roots, "JVM command tree conflicts with backend commands; forwarding it unchanged");
                self.catalog = None;
                self.tree_received = false;
                return encode_packet(jvm).map_err(invalid_data);
            }
        };
        let tree = catalog.merge(|id, _| self.allowed.contains(id)).map_err(invalid_data)?;
        self.catalog = Some(catalog);
        self.tree_received = true;
        encode_packet(&tree).map_err(invalid_data)
    }
    pub fn refresh(&mut self) {
        if self.refreshing || self.descriptors.is_empty() {
            return;
        }
        let Some(origin) = self.state.borrow().clone() else {
            return;
        };
        let Ok(permit) = self.tasks.capacity.clone().try_acquire_owned() else {
            return;
        };
        self.refreshing = true;
        let tasks = self.tasks.clone();
        let descriptors = self.descriptors.clone();
        tasks.platform.cleanup.clone().spawn(async move {
            let _permit = permit;
            let result = tokio::select! {
                () = tasks.connection.cancelled() => return,
                () = origin.cancellation.cancelled() => return,
                result = async { origin.inspect(&tasks.platform).await?; backend::catalog(&tasks.platform, &origin.scope, &descriptors).await } => result,
            };
            let _ = tasks.output.send(Output::Permissions { scope: origin.scope.scope_id, result }).await;
        });
    }
    pub async fn receive(&mut self) -> Output {
        self.output.recv().await.expect("connection retains output sender")
    }
    pub async fn publish<S: tokio::io::AsyncWrite + tokio::io::AsyncRead + Unpin>(
        &mut self,
        output: Output,
        public: &mut Transport<S>,
    ) -> io::Result<()> {
        match output {
            Output::Permissions { scope, result } => {
                if self.origin.as_ref().is_none_or(|origin| origin.scope.scope_id != scope) {
                    return Ok(());
                }
                self.refreshing = false;
                let allowed = match result {
                    Ok(allowed) => allowed,
                    Err(error) => {
                        tracing::debug!(%error, "command permission refresh failed; keeping last permissions");
                        return Ok(());
                    }
                };
                if self.allowed == allowed {
                    return Ok(());
                }
                self.allowed = allowed;
                if self.tree_received
                    && let Some(catalog) = &self.catalog
                {
                    public
                        .write_packet(&catalog.merge(|id, _| self.allowed.contains(id)).map_err(invalid_data)?)
                        .await?;
                }
            }
            Output::Packets { origin, invocation, follow, packets, acknowledgment } => {
                let live = if invocation.is_cancelled() {
                    Err(invalid_data("command invocation canceled"))
                } else {
                    self.tasks.current(&origin, follow)
                };
                let result = match live {
                    Ok(_) => {
                        async {
                            for packet in packets {
                                public.write_encoded(&packet).await?;
                            }
                            Ok(())
                        }
                        .await
                    }
                    Err(error) => Err(error),
                };
                let fatal = result.as_ref().err().is_some_and(|error| error.kind() != io::ErrorKind::InvalidData);
                if let Some(acknowledgment) = acknowledgment {
                    let _ = acknowledgment.send(result);
                }
                if fatal {
                    return Err(io::Error::other("command effect transport failed"));
                }
            }
        }
        Ok(())
    }
}
impl Drop for Commands {
    fn drop(&mut self) {
        self.tasks.connection.cancel();
        self.configuration();
    }
}

#[cfg(test)]
mod fixture;
#[cfg(test)]
mod tests;
