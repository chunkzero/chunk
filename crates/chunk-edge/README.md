# chunk-edge

The edge runs in front of a [management](../../packages/management/README.md) install and accepts every player
connection. It reads the hostname from each connection's handshake and hands the connection to one of that environment's
[gateways](../chunk-proxy/README.md), behind a PROXY protocol v2 header that carries the player's address. It answers
server-list pings itself, and wakes sleeping environments when a player logs in. It keeps no state; its routing table
streams from management.

## Configuration

| Variable                     | Default         | Meaning                                                                                        |
| ---------------------------- | --------------- | ---------------------------------------------------------------------------------------------- |
| `CHUNK_MANAGEMENT_URL`       | required        | The management service.                                                                        |
| `CHUNK_EDGE_TOKEN`           | required        | The token the edge calls management's `EdgeService` with; management's own `CHUNK_EDGE_TOKEN`. |
| `CHUNK_BIND`                 | `0.0.0.0:25565` | The player listener.                                                                           |
| `CHUNK_HANDSHAKE_TIMEOUT_MS` | `5000`          | How long a client gets for its handshake, and for each step of a status exchange.              |
| `CHUNK_WAKE_TIMEOUT_MS`      | `25000`         | How long a login waits for a sleeping environment to wake.                                     |
| `RUST_LOG`                   | `info`          | The log filter.                                                                                |

Gateways accept the edge's PROXY headers only from addresses in their `CHUNK_TRUSTED_EDGES`, which management sets from
its `CHUNK_MACHINE_TRUSTED_EDGES`. [`deploy/compose`](../../deploy/compose/README.md) runs the edge at a static address
for this.

## Behavior

- **Routing.** `EdgeService.WatchRoutes` streams each environment's hostname, gateways and sleep state; the edge keeps
  using its last table while management is unreachable and reconnects with backoff. Hostnames are compared in lower
  case, without a port, trailing dot or Forge suffix. A connection for an unknown hostname is closed without a reply.
  Logins pick a gateway by the client's port and fail over to the others.
- **Pings.** While an environment runs, the edge asks one of its gateways for its status at most once every five seconds
  and shares the answer between pings. While it sleeps, the edge answers with the status core last reported, with zero
  players online, or wakes it first when the environment's sleeping-ping mode says so.
- **Wake on login.** A login to a sleeping environment calls `EdgeService.Wake` and holds the connection until a gateway
  is listed or the wake timeout passes. A refused wake, for example for a client that recently failed authentication,
  ends the login with a message saying the server is sleeping or starting.
- **Limits.** At most 8192 connections may be open before being handed to a gateway, and 32 per client address (per /64
  for IPv6).

## Image and tests

`just edge-image` builds `chunk-edge:<workspace version>` from `Dockerfile`. `cargo test -p chunk-edge` runs the edge
against a fake management service and fake gateways.
