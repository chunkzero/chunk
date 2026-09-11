# Java backend contracts

This Java 21 library supplies Jackson bindings, value checks, and typed references used by generated clients. Its public
runtime dependency is Jackson; it has no Kotlin dependency.

Run `chunk gen PROJECT --target java --java-package com.example.backend` to compile backend declarations and generate
`java/<package>/BackendTypes.java` and the asynchronous Java client. Outputs default to `PROJECT/.chunk/generated/java`.
The [Gradle plugin](../gradle-plugin/README.md) handles generation and source roots automatically when building app
projects. Generation itself needs no JVM compilation. Only public functions receive references. Java namespaces mirror
backend paths: `shared/players/stats` becomes `BackendTypes.Shared.Players.stats`, with `StatsArgs` and `StatsResult`
records beside it when those schemas are objects. Java and Kotlin reserved names and helper conflicts receive an
underscore suffix. Generation rejects ambiguous normalized names, unsafe literals and literals exceeding Java's string
constant limit before writing sources. Wire paths and JSON field names remain unchanged. Select
`chunk gen PROJECT --target typescript` to generate TypeScript instead. TypeScript references preserve nested namespaces
and editable document fields with readonly `_id`.

Objects become Java records, arrays become lists, and `v.enum("allow", "deny")` becomes a Java enum. Named unions such
as `v.union({ready: v.object({}), waiting: v.object({reason: v.string()})})` become sealed interfaces with `Ready` and
`Waiting` records. Their wire forms are `{"type":"ready"}` and `{"type":"waiting","reason":"busy"}`. Jackson annotations
provide field binding and discriminator dispatch; generated models contain no JSON read/write codecs. Shared Jackson
guards require exact enum strings and object-shaped unions with string discriminators.

Documents live in `BackendTypes.Documents`. Each table gets a distinct string ID record in `BackendTypes.Ids`.
`PlayerId` and `SessionId` remain distinct types with shared format validation. Optional API fields use nullable record
components: omission and explicit null both become Java null and serialize as omission. `v.nullable(...)` allows null
for a required value; a missing required property still fails. `v.null()` uses `Void` and Java null. Database patch
omission and `unset` keep their existing meanings.

Generated constructors fail on invalid required values, IDs, malformed Unicode, nonfinite numbers, and integers outside
JavaScript's safe range. Jackson rejects unknown fields and scalar coercions, including fractional JSON numbers for
integer fields. Function references retain Jackson type information and shared checks for scalar and list roots.

Removing this library's direct Gson dependency does not remove Gson from a full gameplay JVM: `grpc-core` and Minestom
still use it. The protobuf module alone has no Gson dependency. The Gradle plugin's build-time JSON code also still uses
Gson and is separate from this runtime API.

`./gradlew :jvm:backend-api:test` generates and compiles fixtures before checking Java round trips.
`cargo test -p chunk-build generated_typescript` checks the same values through generated TypeScript references and
type-checks the emitted module. Repository fixtures can also use `chunk-codegen` directly with an existing contract.
