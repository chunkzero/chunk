# chunk-proxy

The gateway: the listener players connect to. It answers server-list pings, authenticates players with Mojang, owns
their connection's encryption and compression, and relays each player to a session in a gameplay JVM over a native
Minecraft connection. It can move a player between sessions and JVMs without reconnecting them. It supports Minecraft
Java Edition 26.2 (protocol 776), selected by the default `mc-26-2` feature; without a version feature the proxy refuses
to start.

The gateway runs in the [environment process](../chunk-environment/README.md), next to core or alone on a gateway
machine, which configures it through `CHUNK_BIND`, `CHUNK_MOTD`, `CHUNK_MAX_CONNECTIONS`,
`CHUNK_CONNECTION_TIMEOUT_SECONDS`, `CHUNK_TRUSTED_EDGES` and `CHUNK_OFFLINE_LOGINS`, and in `chunk dev`, which uses its
own flags. It reaches core over the `chunk.sync.v1` `Core` service with the credential core minted for it.

## Logins

With a `Config::platform` naming core, the gateway's identity and the deployment to route with:

1. A server-list ping runs the project's `server.ping` hooks through core, and core reports the answer so the
   [edge](../chunk-edge/README.md) can reuse it while the environment sleeps.
2. A login is authenticated with Mojang over HTTPS (five-second timeout); the UUID and profile come from Mojang, never
   from the client, and a failed check never falls back to an offline identity.
3. The player waits in the configuration phase while the `player.login` and `player.route` hooks pick a destination and
   control places them (`chunk:claim`, see [control](../chunk-control/README.md#placing-players)). Admission and
   preparation get 45 seconds, and arrival in the session 20 seconds more.
4. The gateway opens a dedicated connection to the session's JVM with the capability control returned, and relays the
   player's packets from then on.

`Proxy::retarget` returns a handle whose `replace` routes later logins with another deployment; established connections
keep theirs. Core's current deployment changes this way.

## Moves

A move keeps the player's socket, authentication, encryption and compression, across JVMs too. The destination's
`player.login` and `player.beforeMove` hooks approve it. The gateway reserves the destination, withdraws the source
claim before logging in to the destination, discards late output from the source, takes the client back to the
configuration phase, keeps its latest settings, and relays the destination's configuration. If preparation fails, the
player keeps playing on the source. A cutover that can't finish ends within a bounded deadline; it doesn't replay
packets or roll back.

## Limits and settings

- `CHUNK_MAX_CONNECTIONS` (default 1024) caps live connections; further accepts are dropped.
- Each login exchange, authentication included, has a ten-second deadline (`Config::connection_timeout`,
  `CHUNK_CONNECTION_TIMEOUT_SECONDS`). Each configuration phase has five minutes (`Config::configuration_timeout`), and
  clients must send their settings within ten seconds. While a player waits there, keepalives go out ten seconds after
  the last answer, and each gets fifteen seconds. Writes have a five-second deadline.
- Compression starts at 256 bytes; `Config::compression_threshold = None` turns it off.
- `CHUNK_TRUSTED_EDGES` lists edge IPs or CIDRs whose connections must open with a PROXY protocol v2 header; the
  header's source becomes the player's address. Connections from other addresses are never parsed for one.
- `CHUNK_OFFLINE_LOGINS=1` (`Config::offline_logins`) skips encryption and Mojang, as vanilla offline mode does. It is
  insecure; use it only for local tests.
- SIGTERM or Ctrl-C closes the listener and active connections. `RUST_LOG=debug` logs individual connection failures.

A library caller can leave `Config::platform` unset; the gateway then sends every player to a waiting world (an empty
End sky over 5×5 empty chunks) and disconnects them sixty seconds after login. The tests use it.

## Testing

`cargo test -p chunk-proxy` runs the listener tests, and `cargo test -p chunk-proxy --no-default-features` checks the
build without a version feature. Packet codecs are documented in [`chunk-protocol`](../chunk-protocol/src/lib.rs).
