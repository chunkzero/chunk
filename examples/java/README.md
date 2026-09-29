# Java example

A minimal Java project that shows the Java side of the API with no Kotlin dependency. It builds a release but has no
routing, so players can't join it; run the [Kotlin example](../local/README.md) to play.

- [`apps/lobby/app.ts`](apps/lobby/app.ts) declares the `lobby` app with one implementation whose creation config is a
  `greeting` string.
- [`Lobby.java`](apps/lobby/src/main/java/example/Lobby.java) is the app's `main` and its `@SessionType("default")`
  provider. It implements the generated `LobbySessionProviders.Default` interface to receive that config, and its
  session implements the `announce` session method declared in
  [`apps/lobby/server/methods.ts`](apps/lobby/server/methods.ts).
- [`LobbyComponents.java`](apps/lobby/src/main/java/example/LobbyComponents.java) declares the session's `BackendClient`
  as a component.
- On join, the session calls the `shared/greetings/message` query from [`server/greetings.ts`](server/greetings.ts) and
  sends the result on the tick thread.

The Java builds compile with `-Xlint:all -Werror`.

## Build

From the repository root:

```sh
just toolchain
target/debug/chunk build examples/java
```

The release appears as `examples/java/dist/<id>/` and `examples/java/dist/<id>.tar.gz`. The Gradle settings include the
Gradle plugin and the JVM libraries from this checkout as composite builds, so the example always builds against the
current sources.

`just consumers` builds this project and the Kotlin example from source-only scratch copies and checks their release
archives, session registries and the Java/Kotlin dependency boundary, without starting any services.
