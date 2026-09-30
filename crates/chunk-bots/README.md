# chunk-bots

Load bots: real Minecraft 26.2 clients, many per process, for load runs against a gateway or an edge. Each bot logs in
offline, so the target must admit offline logins (`chunk dev --offline-logins`, `CHUNK_OFFLINE_LOGINS=1` on a gateway,
or `CHUNK_MACHINE_OFFLINE_LOGINS=1` on management). It never authenticates with Mojang and never encrypts.

```sh
cargo build --release -p chunk-bots
target/release/chunk-bots --address 127.0.0.1:25566 --bots 200 --login-rate 20 --hold 120
target/release/chunk-bots --address play.example.com:25565 --bots 1000 --first 1000 --command coin --json
```

`--help` lists every option. [`scripts/load/run.py`](../../scripts/load/run.py) runs bots from several hosts.

## What a bot does

1. Connects at the ramp's `--login-rate`, with at most `--max-pending` (default 32, the edge's per-address limit)
   connections still logging in, and sends a handshake naming `--hostname` and a login as `<prefix><index>`.
2. Follows compression, acknowledges the login, sends client information, answers known packs, keepalives, pings and the
   code of conduct, and acknowledges the end of configuration, as the gateway and Minestom expect of a client.
3. In play, confirms teleports, reports itself loaded and acknowledges chunk batches. The first teleport ends its login;
   `login_to_play_ms` measures from connecting until then.
4. Then walks a three-block circle around where it was placed at `--move-hz` (default 20, a moving vanilla client),
   sends a play ping every `--ping-interval` seconds (the gateway relays it to the JVM, so `ping_rtt_ms` includes
   Minestom's tick), and runs `--command` every `--command-interval` seconds if given.
5. Follows moves between sessions: acknowledges reconfiguration and plays on.

A bot that fails before play counts under `failed`, one disconnected afterwards under `disconnects`, each with its
reason. Bots don't reconnect. A run ends `--hold` seconds after the ramp, or on Ctrl-C, and prints its summary: counts,
login and ping percentiles in milliseconds, bytes received and the process's CPU. Progress lines go to stderr every
`--stats-interval` seconds.

To keep bot CPU low, frames the bot doesn't read are skipped as they arrive; compressed ones are inflated only far
enough to read their packet ID, so chunk and registry data is never decoded. Raise the open-file limit (`ulimit -n`) for
more than about a thousand bots per process.
