use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    rc::Rc,
    sync::Arc,
};

use chunk_js::DeploymentId;
use chunk_store::Revision;
use tokio::sync::watch;

use super::index::{QueryId, ReadIndex};
use crate::{
    Error, Result,
    reads::{Change, Dependencies, View},
    service::{Call, GroupSubscription, GroupUpdate, Request},
    timing::{Phase, Timer},
};

type GroupId = u64;

const GROUP_RESULT_BYTES: usize = 1024 * 1024;

/// Subscriptions with the same identity share one evaluation. The caller is part of
/// the identity only after an evaluation read it.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Identity {
    deployment: DeploymentId,
    function: String,
    arguments: String,
    caller: Option<String>,
}

impl Identity {
    fn new(call: &Call, caller: bool) -> Self {
        Self {
            deployment: call.deployment.clone(),
            function: call.function.clone(),
            arguments: call.arguments.as_str().to_owned(),
            caller: caller.then(|| call.caller.as_str().to_owned()),
        }
    }
}

struct Query {
    identity: Identity,
    /// Evaluation input; its caller belongs to one of the subscribers.
    call: Call,
    reads: Dependencies,
    result: Option<Result<Arc<str>>>,
    /// Unique per stored result, so groups can tell what changed.
    version: u64,
    /// Snapshot revision the stored result ran against.
    evaluated: Revision,
    /// The stored result is valid from its evaluation until this commit.
    stale: Option<Revision>,
    /// First commit since the current batch started that affected this query; it runs in the next batch.
    dirty: Option<Revision>,
    /// Queued or running in the current batch.
    scheduled: bool,
    /// Positions per group that subscribe to this query.
    groups: BTreeMap<GroupId, usize>,
}

struct Group {
    calls: Vec<Call>,
    queries: Vec<QueryId>,
    versions: Vec<u64>,
    revision: Revision,
    sender: Option<watch::Sender<Result<GroupUpdate>>>,
    /// Held until every query has a result for the initial update.
    reply: Option<Request<GroupSubscription>>,
}

impl Group {
    fn closed(&self) -> bool {
        match &self.sender {
            Some(sender) => sender.is_closed(),
            None => self.reply.as_ref().is_none_or(|reply| reply.cancellation.is_cancelled()),
        }
    }
}

/// Every query in a batch runs against the same durable snapshot, so each group
/// can publish a consistent update when the batch completes, however commits interleave.
struct Batch {
    generation: u64,
    view: Rc<View>,
    queue: VecDeque<QueryId>,
    running: usize,
    /// Commit acknowledgments this batch covers.
    commits: Vec<Timer>,
}

pub(super) struct Job {
    pub id: QueryId,
    pub call: Call,
    pub view: Rc<View>,
    generation: u64,
}

pub(super) struct Watches {
    queries: BTreeMap<QueryId, Query>,
    identities: BTreeMap<Identity, QueryId>,
    groups: BTreeMap<GroupId, Group>,
    index: ReadIndex,
    batch: Option<Batch>,
    /// Queries marked dirty for the next batch; entries may have been removed since.
    next: Vec<QueryId>,
    next_commits: Vec<Timer>,
    /// Commits after the current batch's snapshot. A completed evaluation checks its
    /// new reads against them, since the index only knew its previous reads.
    recent: Vec<(Revision, Option<Arc<[Change]>>)>,
    durable: Revision,
    ids: u64,
}

impl Watches {
    pub fn new(durable: Revision) -> Self {
        Self {
            queries: BTreeMap::new(),
            identities: BTreeMap::new(),
            groups: BTreeMap::new(),
            index: ReadIndex::default(),
            batch: None,
            next: Vec::new(),
            next_commits: Vec::new(),
            recent: Vec::new(),
            durable,
            ids: 0,
        }
    }

    fn id(&mut self) -> u64 {
        self.ids += 1;
        self.ids
    }

    pub fn len(&self) -> usize {
        self.groups.len()
    }

    pub fn references(&self, deployment: &DeploymentId) -> bool {
        self.groups.values().any(|group| group.calls.iter().any(|call| &call.deployment == deployment))
    }

    pub fn has_work(&self) -> bool {
        match &self.batch {
            Some(batch) => !batch.queue.is_empty(),
            None => !self.next.is_empty(),
        }
    }

    /// Registers a group. The reply completes once every query has a result.
    pub fn subscribe(&mut self, calls: Vec<Call>, reply: Request<GroupSubscription>) {
        let group = self.id();
        let queries = calls.iter().map(|call| self.attach(call, group)).collect();
        self.groups.insert(
            group,
            Group { calls, queries, versions: Vec::new(), revision: Revision(0), sender: None, reply: Some(reply) },
        );
        self.publish(group);
    }

    fn attach(&mut self, call: &Call, group: GroupId) -> QueryId {
        let id = [Identity::new(call, true), Identity::new(call, false)]
            .iter()
            .find_map(|identity| self.identities.get(identity).copied())
            .unwrap_or_else(|| self.create(call, false));
        *self.queries.get_mut(&id).expect("attached query").groups.entry(group).or_default() += 1;
        id
    }

    fn create(&mut self, call: &Call, caller: bool) -> QueryId {
        let id = self.id();
        let identity = Identity::new(call, caller);
        self.identities.insert(identity.clone(), id);
        self.queries.insert(
            id,
            Query {
                identity,
                call: call.clone(),
                reads: Dependencies::default(),
                result: None,
                version: 0,
                evaluated: Revision(0),
                stale: None,
                dirty: None,
                scheduled: false,
                groups: BTreeMap::new(),
            },
        );
        self.schedule(id);
        id
    }

    /// Runs a new query in the current batch, or starts the next one.
    fn schedule(&mut self, id: QueryId) {
        let query = self.queries.get_mut(&id).expect("scheduled query");
        if let Some(batch) = &mut self.batch {
            query.scheduled = true;
            batch.queue.push_back(id);
        } else if query.dirty.is_none() {
            query.dirty = Some(self.durable);
            self.next.push(id);
        }
    }

    /// Marks the queries a durable commit affected for the next batch.
    pub fn changed(&mut self, revision: Revision, changes: Arc<[Change]>) {
        self.durable = revision;
        let mut affected = BTreeSet::new();
        self.index.affected(&changes, &mut affected);
        for id in &affected {
            self.invalidate(*id, revision);
        }
        if self.batch.is_some() {
            self.recent.push((revision, Some(changes)));
        }
        if !affected.is_empty() {
            self.next_commits.push(Timer::start());
        }
    }

    /// A schema activation invalidates every query and restarts at the new snapshot.
    pub fn barrier(&mut self, revision: Revision) {
        self.durable = revision;
        if let Some(batch) = self.batch.take() {
            self.next_commits.extend(batch.commits);
        }
        self.recent.clear();
        let ids: Vec<_> = self.queries.keys().copied().collect();
        for id in ids {
            let query = self.queries.get_mut(&id).expect("query");
            query.scheduled = false;
            self.invalidate(id, revision);
        }
    }

    fn invalidate(&mut self, id: QueryId, revision: Revision) {
        let Some(query) = self.queries.get_mut(&id) else {
            return;
        };
        query.stale.get_or_insert(revision);
        if query.dirty.is_none() {
            query.dirty = Some(revision);
            self.next.push(id);
        }
    }

    /// Takes the next evaluation, starting a batch against `latest` when none is running.
    pub fn next_job(&mut self, latest: impl FnOnce() -> Rc<View>) -> Option<Job> {
        if self.batch.is_none() && !self.next.is_empty() {
            self.start(latest());
        }
        let batch = self.batch.as_mut()?;
        while let Some(id) = batch.queue.pop_front() {
            if let Some(query) = self.queries.get(&id) {
                batch.running += 1;
                return Some(Job {
                    id,
                    call: query.call.clone(),
                    view: batch.view.clone(),
                    generation: batch.generation,
                });
            }
        }
        self.finish();
        None
    }

    fn start(&mut self, view: Rc<View>) {
        let mut queue = VecDeque::new();
        for id in std::mem::take(&mut self.next) {
            if let Some(query) = self.queries.get_mut(&id)
                && query.dirty.take().is_some()
            {
                query.scheduled = true;
                queue.push_back(id);
            }
        }
        if queue.is_empty() {
            for commit in std::mem::take(&mut self.next_commits) {
                commit.stop(Phase::FanOut);
            }
            return;
        }
        self.recent.clear();
        self.batch = Some(Batch {
            generation: self.id(),
            view,
            queue,
            running: 0,
            commits: std::mem::take(&mut self.next_commits),
        });
    }

    fn finish(&mut self) {
        if self.batch.as_ref().is_some_and(|batch| batch.queue.is_empty() && batch.running == 0) {
            let batch = self.batch.take().expect("finished batch");
            for commit in batch.commits {
                commit.stop(Phase::FanOut);
            }
            self.recent.clear();
        }
    }

    pub fn complete(&mut self, job: &Job, result: Result<Arc<str>>, reads: Dependencies) {
        let (id, generation) = (job.id, job.generation);
        let Some(batch) = self.batch.as_mut().filter(|batch| batch.generation == generation) else {
            return;
        };
        batch.running -= 1;
        let evaluated = batch.view.revision;
        if self.queries.contains_key(&id) {
            self.store(id, evaluated, result, reads);
        }
        self.finish();
    }

    fn store(&mut self, id: QueryId, evaluated: Revision, result: Result<Arc<str>>, reads: Dependencies) {
        let missed = self
            .recent
            .iter()
            .find(|(_, changes)| changes.as_ref().is_none_or(|changes| reads.affected(changes)))
            .map(|(revision, _)| *revision);
        let version = self.id();
        let query = self.queries.get_mut(&id).expect("stored query");
        query.scheduled = false;
        // Commits that marked this query dirty during the batch all follow its snapshot.
        query.stale = query.dirty.into_iter().chain(missed).min();
        if query.dirty.is_none() && missed.is_some() {
            query.dirty = missed;
            self.next.push(id);
        }
        self.index.remove(id, &query.reads);
        self.index.insert(id, &reads);
        let split = reads.caller && query.identity.caller.is_none();
        query.reads = reads;
        query.evaluated = evaluated;
        if query.result.as_ref().is_none_or(|previous| !same(previous, &result)) {
            query.result = Some(result);
            query.version = version;
        }
        let groups: Vec<_> = query.groups.keys().copied().collect();
        if split {
            self.split(id);
        }
        for group in groups {
            self.publish(group);
        }
    }

    /// The query read its caller, so its result only holds for subscribers with that caller.
    /// The others move to queries keyed by their own caller and run in this batch.
    fn split(&mut self, id: QueryId) {
        let query = self.queries.get_mut(&id).expect("split query");
        // Detaching may remove this query when its own caller already unsubscribed.
        let caller = query.call.caller.as_str().to_owned();
        let identity = Identity::new(&query.call, true);
        let groups: Vec<_> = query.groups.keys().copied().collect();
        self.identities.remove(&query.identity);
        query.identity = identity.clone();
        self.identities.insert(identity, id);
        for group in groups {
            for position in 0..self.groups[&group].queries.len() {
                let entry = &self.groups[&group];
                if !self.queries.contains_key(&id) {
                    return;
                }
                if entry.queries[position] != id || entry.calls[position].caller.as_str() == caller {
                    continue;
                }
                let call = entry.calls[position].clone();
                let target = match self.identities.get(&Identity::new(&call, true)) {
                    Some(target) => *target,
                    None => self.create(&call, true),
                };
                *self.queries.get_mut(&target).expect("split target").groups.entry(group).or_default() += 1;
                self.groups.get_mut(&group).expect("split group").queries[position] = target;
                self.detach(id, group);
            }
        }
    }

    /// Publishes the group if its queries agree on a revision and any result changed.
    fn publish(&mut self, id: GroupId) {
        let group = &self.groups[&id];
        let mut first = Revision(0);
        let mut last = self.durable;
        for query in &group.queries {
            let query = &self.queries[query];
            if query.result.is_none() {
                return;
            }
            first = first.max(query.evaluated);
            if let Some(stale) = query.stale {
                last = last.min(Revision(stale.0.saturating_sub(1)));
            }
        }
        if first > last || last < group.revision {
            return;
        }
        let versions: Vec<_> = group.queries.iter().map(|query| self.queries[query].version).collect();
        if group.sender.is_some() && versions == group.versions {
            return;
        }
        let mut bytes = 0;
        let results = group
            .queries
            .iter()
            .map(|query| {
                self.queries[query].result.clone().expect("published result").and_then(|json| {
                    bytes += json.len();
                    if bytes > GROUP_RESULT_BYTES {
                        return Err(Error::Invalid("query group result limit"));
                    }
                    Ok(json)
                })
            })
            .collect();
        let update = GroupUpdate { revision: last, results };
        let group = self.groups.get_mut(&id).expect("published group");
        group.versions = versions;
        group.revision = last;
        if let Some(sender) = &group.sender {
            let _ = sender.send_replace(Ok(update));
        } else if let Some(reply) = group.reply.take() {
            let (sender, receiver) = watch::channel(Ok(update));
            group.sender = Some(sender);
            reply.finish(Ok(GroupSubscription::new(receiver)));
        }
    }

    /// Drops groups whose subscribers went away, and queries nobody subscribes to.
    pub fn sweep(&mut self) {
        let closed: Vec<_> = self.groups.iter().filter(|(_, group)| group.closed()).map(|(id, _)| *id).collect();
        for id in closed {
            let group = self.groups.remove(&id).expect("closed group");
            for query in group.queries {
                self.detach(query, id);
            }
        }
    }

    fn detach(&mut self, id: QueryId, group: GroupId) {
        let query = self.queries.get_mut(&id).expect("detached query");
        let positions = query.groups.get_mut(&group).expect("detached group");
        *positions -= 1;
        if *positions == 0 {
            query.groups.remove(&group);
        }
        if query.groups.is_empty() {
            let query = self.queries.remove(&id).expect("unsubscribed query");
            if self.identities.get(&query.identity) == Some(&id) {
                self.identities.remove(&query.identity);
            }
            self.index.remove(id, &query.reads);
        }
    }

    pub fn fail(&mut self, error: &Error) {
        for (_, group) in std::mem::take(&mut self.groups) {
            if let Some(sender) = group.sender {
                let _ = sender.send_replace(Err(error.clone()));
            } else if let Some(reply) = group.reply {
                reply.finish(Err(error.clone()));
            }
        }
        *self = Self::new(self.durable);
    }
}

fn same(a: &Result<Arc<str>>, b: &Result<Arc<str>>) -> bool {
    match (a, b) {
        (Ok(a), Ok(b)) => a == b,
        (Err(a), Err(b)) => a.to_string() == b.to_string(),
        _ => false,
    }
}
