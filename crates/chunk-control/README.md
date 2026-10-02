# chunk-control

Control is core's placement side. It reserves capacity, places sessions on hosts, starts and supervises their JVMs, and
owns which player is delivered to which session. It runs inside core, next to the [backend](../chunk-backend/README.md),
in the [environment process](../chunk-environment/README.md) and in `chunk dev`; it has no binary of its own. Gateways,
JVMs and the CLI reach it through core's `chunk.sync.v1` `Core` service.

## Model

- A **release** is a deployment version's apps, session types, machine profiles and limits, and the asset revision it
  pins with the resource packs its players get. `Control::activate_release` records one and makes it current;
  `Control::retire_release` stops placing on an earlier one and stops its hosts.
- A **host** is one capacity request and one JVM lifetime, never relaunched. A JVM belongs to one environment, release,
  app and machine profile, and can run several sessions. Control creates hosts through the `Host` trait: `ProcessHost`
  runs each JVM as a local child process (`chunk dev`), with `CHUNK_ASSETS` naming its app's directory materialized from
  the configured asset store, and the environment's runner host asks management for a machine running
  [`chunk-jvm`](../chunk-jvm/README.md). Each host follows its own `jvm/<host>` topic.
- A **session** runs one of an app's session types on a host. Compatible demand shares a session up to its declared
  capacity. An app ends a session through its session scope; control then retires it.
- A **claim** is a player's reservation of a slot in a session, and later the player's ownership of it.

Control keeps its state in `chunk_`-prefixed system tables in the backend's store (`chunk_claims`, `chunk_hosts`,
`chunk_sessions` and others), so an environment has one log. Each update is one commit through the backend's system
lane, ahead of queued app commits, and an in-memory copy serves reads. Apps can't read or write `chunk_` tables. One
control runs per environment and holds its state exclusively; when the backend's commit pipeline stops, as after another
store fences this one, control stops too.

## Placing players

A gateway admits a player with `chunk:claim`, naming the player, its connection, the release it routed the login with,
and a session demand (session type, key and machine profile). Control reserves capacity, starts a JVM if needed, waits
until the session is ready, and returns the session's endpoint and a single-use capability for a native Minecraft
connection to it. The gateway then records its intent to deliver with `chunk:activate` and connects; `chunk:withdraw`
gives the claim back, and `chunk:depart` ends it when the player leaves. Unactivated reservations expire after 60
seconds. Active ownership never expires just because a gateway's stream is unavailable, and a duplicate login is refused
while an earlier owner is unresolved.

A login is placed on the release its gateway routed it with, or the current one. A login routed with a retired release
is refused, and the gateway routes it again. Moves and existing sessions stay on their host's release.

Each claim carries a generation, the `(epoch, revision)` of the commit that created it. Generations compare as pairs,
because a restore starts a new epoch and may reuse revisions. They fence stale work: an old cancellation can't release a
newer connection, and a JVM's deliveries are checked against them.

**Moves.** A move (`chunk:move_player` from the operator, a player command, an action, or `chunk:move` from the JVM
hosting the player) reserves the destination without creating a second player. Operator and JVM moves name the player's
current arrived claim, and a JVM may name only a claim on its own host. Each goes through the destination's admission
policy, and a refused move reports why: the player is offline, the claim is stale, the destination is full, or it is
unknown. The gateway withdraws the source claim before activating the destination, so a player is never delivered twice.
`Control::move_roster` moves a group into one session: it reserves every slot and queues every member's move in one
commit, or changes nothing, and admits the members together once all have asked to activate.

**Nodes and drains.** The operator's `nodes` topic reports each host as starting, online, unhealthy, unreachable,
draining, stopping or stopped, with the JVM's last health report. Health is checked every five seconds: a stalled tick
loop blocks new placement, and three failed checks in a row stop the JVM. `chunk:drain` takes a host, or a player's
host, and a deadline: it stops new placement there, moves its players off, and stops the JVM once it is empty or at the
deadline. A host with no unfinished session is stopped after the release's idle timeout (default 60 seconds).

## Recovery

Control records reservations, activation intent and ownership before any external effect, and keeps a claim whose
cleanup it can't confirm. Before spawning a JVM, `ProcessHost` publishes a launch marker with the process's identity and
the digest of its credential, locked exclusively; the JVM inherits the lock as file descriptor 3. A JVM keeps repeating
its registration, and control accepts it again only if the marker matches. A launch whose JVM may still run is confirmed
exited only once control can take the marker's lock.

After control opens, and whenever a JVM re-attaches, new claims fail as busy until every surviving JVM is fenced or
confirmed exited. Fencing withdraws the deliveries no open claim matches. A JVM whose release the log lost is only ever
stopped. While a launch stays unresolved, control logs a warning every 30 seconds. A lost JVM loses its live worlds;
nothing replays packets to restore them.

## Limits

A release allows at most 32 processes. A machine profile allows 1 to 16 sessions per process, a session type 1 to 128
players, and a host at most 128 players across its sessions. Control retains up to 256 sessions. At most 1024 claim,
activation and cancellation operations are in flight; beyond that, calls fail with `OVERLOADED` and should be retried.
Released claims are forgotten five minutes after release.

## Session methods

A command can call a method on the session it started in. `Control::capture_session` binds an arrived claim,
`prepare_session_method` checks the method against the release's declared session methods and freezes its target,
arguments, deadline and a new operation ID, and `call_session_method` puts it on the JVM's topic and waits for the
result. Retries must reuse the same prepared operation. Arguments and results are JSON of at most 48 KiB, and deadlines
range from 1 ms to 30 seconds. Control keeps each result for five minutes, at most 256 methods and 8 MiB per JVM, and
remembers 65,536 finished operation IDs per JVM so none runs twice. An outcome is completed, cancelled (gameplay did not
start), failed (gameplay threw or returned an invalid result) or unknown; neither failure nor unknown implies rollback.

## Testing

`cargo test -p chunk-control`.
