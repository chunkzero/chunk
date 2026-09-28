# Java consumer

This build fixture uses the public Java session API and generated typed backend client, with `-Xlint:all -Werror` and no
Kotlin production dependencies. Its lobby provider implements the generated `LobbySessionProviders.Default` interface
and receives a typed greeting configuration declared in `apps/lobby/app.ts`. The session join hook queries a typed
greeting and sends the result on the tick thread. It also implements the generated `Announce` session-method interface
from `apps/lobby/server/methods.ts`; the generated binding sends a message to that session’s players. Internal control
dispatch binds calls to the current player membership and session.

From the repository root:

```sh
just toolchain
target/debug/chunk build examples/java
```

The complete release appears under `examples/java/dist/<id>/` with a sibling `<id>.tar.gz` archive. The settings use
repository composite builds for the plugin and framework libraries; the consumer plugin uses the prepared CLI directly.
This fixture has no local admission/routing configuration. Use the [Kotlin local example](../local/README.md) to run
gameplay.

`just consumers` builds both projects from source-only scratch copies and checks their archives, app registrations,
shared bindings and Java/Kotlin dependency boundaries. With an already prepared CLI, run
`python3 scripts/check-consumers.py /absolute/path/to/chunk` from the repository root. Neither acceptance path starts
gameplay or backend services.
