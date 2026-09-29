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

## Testing

`./gradlew :jvm:backend-api:test` generates its fixtures from `src/test/resources/contract.json` with
`cargo run -p chunk-build --bin chunk-codegen`, so it needs the Rust toolchain, then checks Java round trips.
`cargo test -p chunk-build generated_typescript` checks the same fixtures through generated TypeScript.
