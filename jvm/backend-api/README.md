# Java backend contracts

This Java 21 library supplies the codecs and typed references used by generated
clients. Its public runtime dependency is Gson; it has no Kotlin dependency.

Run `cargo run -p chunk-build --bin chunk-codegen -- java CONTRACT OUTPUT JAVA_PACKAGE`
to generate `java/<package>/BackendTypes.java` and the Java client from the compiler's
contract. Generation needs no JVM compilation. Only public functions receive
references. Java fields use `$` between path segments; reserved names and helper
class names receive a `$` suffix. Generation rejects reference names that collide
with generated codecs, unsafe literals and literals exceeding Java's string
constant limit. Select `typescript CONTRACT OUTPUT` to generate TypeScript instead.
TypeScript references preserve nested namespaces and editable
document fields with readonly `_id`.

Objects become records, arrays become lists, and unions become sealed interfaces
with numbered record variants in declaration order. `FieldValue.Absent` means a
property is omitted; `FieldValue.Present` contains its value. JSON null is the
explicit `NullValue.INSTANCE` union variant, never Java null. Codecs reject extra
properties, invalid IDs, malformed Unicode, nonfinite numbers and integers outside
JavaScript's safe range. Document IDs carry generated table marker types.

`./gradlew :jvm:backend-api:test` generates and compiles fixtures before checking
Java round trips. `cargo test -p chunk-build generated_typescript` checks the same
values through generated TypeScript references and type-checks the emitted module.
