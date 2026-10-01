# Backend API

The Java types that generated backend bindings are built from: function references (`QueryRef`, `MutationRef`,
`SessionMethodRef`), `JsonType` with its strict Jackson configuration (`BackendJson`), and the platform ID types
`PlayerId` and `SessionId`. It is a Java 21 library whose only runtime dependency is Jackson; it has no Kotlin
dependency. Apps don't use it directly: they call functions through the generated
[`BackendClient`](../backend-client/README.md).

## Generated bindings

`chunk gen PROJECT --target java` compiles the project's backend and writes JVM sources to
`PROJECT/.chunk/generated/java`, in the package set by `--java-package` (default `dev.chunkzero.generated`). In an app
project the [Gradle plugin](../gradle-plugin/README.md) does this before compilation, so running it by hand is rarely
needed. `--target kotlin` adds the coroutine client, and `--target typescript` writes TypeScript references instead.

Public queries and mutations get a reference and argument and result types in `BackendTypes`, nested by function path:
`shared/players/stats` becomes `BackendTypes.Shared.Players.stats`, with `StatsArgs` and `StatsResult` records beside it
when those are objects. Actions and internal functions get none. Names that clash with Java or Kotlin keywords get a
trailing underscore; ambiguous names fail generation. Wire paths and JSON field names are unchanged.

| Validator                               | Java type                                                                        |
| --------------------------------------- | -------------------------------------------------------------------------------- |
| `v.object({...})`                       | A record                                                                         |
| `v.array(x)`                            | `List<X>`                                                                        |
| `v.enum("allow", "deny")`               | An enum                                                                          |
| `v.union({ ready: ..., waiting: ... })` | A sealed interface with `Ready` and `Waiting` records, as `{"type":"ready",...}` |
| `v.integer()`, `v.number()`             | `Long`, `Double`                                                                 |
| `v.id("profiles")`                      | `BackendTypes.Ids.Profiles`, one distinct record per table                       |
| `v.player()`, `v.session()`             | `PlayerId`, `SessionId`                                                          |
| `v.optional(x)`, `v.nullable(x)`        | A nullable component; an optional one is omitted from JSON when `null`           |
| `v.null()`                              | `Void`                                                                           |

Documents are records in `BackendTypes.Documents`. Generated constructors reject invalid values, such as bad IDs,
malformed Unicode, non-finite numbers and integers outside JavaScript's safe range, and deserialization rejects unknown
fields and type coercions.

### Variables

`Vars` holds `chunk.toml`'s [variables](../../crates/chunk-build/sdk/README.md#variables-and-secrets) as constants,
named as declared, with keyword clashes renamed as above. A variable `[vars]` sets is a `String`, and one only some
`[env.<name>.vars]` set is an `Optional<String>`, which Kotlin unwraps with `getOrNull()`. The values are embedded and
resolve once, when the class loads, for the environment `CHUNK_ENVIRONMENT_NAME` names, overriding `[vars]` key by key
as the backend does; without it, only `[vars]` apply. Secrets are never generated.

```java
String motd = Vars.MOTD;
Optional<String> store = Vars.STORE_URL; // set in [env.prod.vars] only
```

## Testing

`./gradlew :jvm:backend-api:test` generates its fixtures from `src/test/resources/contract.json` with
`cargo run -p chunk-build --bin chunk-codegen`, so it needs the Rust toolchain, then checks Java round trips.
`cargo test -p chunk-build generated_typescript` checks the same fixtures through generated TypeScript.
