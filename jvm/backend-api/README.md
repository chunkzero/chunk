# Java backend contracts

This Java 21 library supplies the codecs and typed references used by generated clients. Its public runtime dependency
is Gson; it has no Kotlin dependency.

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

Objects become records, arrays become lists, and unions become sealed interfaces with numbered record variants in
declaration order. Nested shapes live inside their owning record or union, such as `RecordArgs.Note.V0`. Each generated
record or union exposes its `CODEC`. Documents live in `BackendTypes.Documents`, with ID markers in
`BackendTypes.Tables`. `FieldValue.Absent` means a property is omitted; `FieldValue.Present` contains its value. JSON
null is the explicit `NullValue.INSTANCE` union variant, never Java null. Codecs reject extra properties, invalid IDs,
malformed Unicode, nonfinite numbers and integers outside JavaScript's safe range. Document IDs carry generated table
marker types.

`./gradlew :jvm:backend-api:test` generates and compiles fixtures before checking Java round trips.
`cargo test -p chunk-build generated_typescript` checks the same values through generated TypeScript references and
type-checks the emitted module. Repository fixtures can also use `chunk-codegen` directly with an existing contract.
