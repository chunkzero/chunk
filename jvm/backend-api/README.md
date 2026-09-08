# Java backend contracts

This Java 21 library supplies the codecs and typed references used by generated
clients. Its public runtime dependency is Gson; it has no Kotlin dependency.

Run `cargo run -p chunk-build --bin chunk-codegen -- CONTRACT OUTPUT JAVA_PACKAGE`
to generate `api.ts` and `java/<package>/BackendTypes.java` from the compiler's
contract. Generation needs no JVM compilation. Only public functions receive
references. Java fields use `$` between path segments; reserved record component
names receive a `$` suffix. TypeScript references preserve nested namespaces.

Objects become records, arrays become lists, and unions become sealed interfaces
with numbered record variants in declaration order. `FieldValue.Absent` means a
property is omitted; `FieldValue.Present` contains its value. JSON null is the
explicit `NullValue.INSTANCE` union variant, never Java null. Codecs reject extra
properties, invalid IDs, malformed Unicode, nonfinite numbers and integers outside
JavaScript's safe range. Document IDs carry generated table marker types.

`./gradlew :jvm:backend-api:test` generates and compiles fixtures before checking
Java round trips. `cargo test -p chunk-build generated_typescript` checks the same
values through generated TypeScript references and type-checks the emitted module.
