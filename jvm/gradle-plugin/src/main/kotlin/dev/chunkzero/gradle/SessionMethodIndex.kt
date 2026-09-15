package dev.chunkzero.gradle

import com.google.gson.Gson
import com.google.gson.JsonParser
import java.io.File

internal fun writeSessionMethods(
    app: String,
    providers: Map<String, String>,
    contract: File,
    lookup: (String) -> CompiledClass?,
    resources: File,
    sources: File,
) {
    val metadata = JsonParser.parseString(contract.readText()).asJsonObject
    require(metadata["version"]?.asInt == 1) { "Unsupported session method metadata version" }
    val methods = requireNotNull(metadata["methods"]?.asJsonArray) { "Missing session methods" }
    val declarations = methods.map { it.asJsonObject }.filter { it["app"].asString == app }

    fun inherits(
        name: String,
        target: String,
        seen: MutableSet<String> = mutableSetOf(),
    ): Boolean =
        name == target || (seen.add(name) && lookup(name)?.parents.orEmpty().any { inherits(it, target, seen) })

    fun creates(
        name: String,
        configured: Boolean,
        seen: MutableSet<String> = mutableSetOf(),
    ): String? {
        if (!seen.add(name)) return null
        val type = lookup(name) ?: return null
        val result = if (configured) type.createsConfigured else type.creates
        return result ?: type.parents.firstNotNullOfOrNull { creates(it, configured, seen) }
    }

    val implementations =
        providers.mapValues { (_, provider) ->
            val type = provider.replace('.', '/')
            creates(type, inherits(type, "dev/chunkzero/runtime/ConfiguredSessionProvider"))
        }
    val bindings =
        declarations.map { method ->
            val session = method["session"].asString
            val name = method["name"].asString
            val implementation = implementations[session]
            require(
                implementation != null && lookup(implementation)?.publicConcrete == true &&
                    inherits(implementation, "dev/chunkzero/runtime/Session"),
            ) {
                "Session $app/$session method $name requires a provider creation method with a public concrete Session return type"
            }
            val contractInterface = method["binary_interface"].asString.replace('.', '/')
            require(inherits(implementation, contractInterface)) {
                "Session $app/$session must implement ${method["interface"].asString} for method $name"
            }
            val type = method["interface"].asString
            "new dev.chunkzero.runtime.SessionMethodBinding<>($type.REF, " +
                "(session, args) -> (($type) session).${method["function"].asString}(args))"
        }
    for ((session, implementation) in implementations) {
        if (implementation == null) continue
        val foreign =
            methods.map { it.asJsonObject }.firstOrNull {
                (it["app"].asString != app || it["session"].asString != session) &&
                    inherits(implementation, it["binary_interface"].asString.replace('.', '/'))
            }
        require(foreign == null) { "Session $app/$session implements a method belonging to another app or session" }
    }
    sources.deleteRecursively()
    sources.mkdirs()
    val service = resources.resolve("META-INF/services/dev.chunkzero.runtime.SessionMethodProvider")
    service.delete()
    if (bindings.isNotEmpty()) {
        val type = "dev.chunkzero.generated.sessions.a${app.toByteArray().joinToString(
            "",
        ) { "%02x".format(it) }}.ChunkSessionMethods"
        val file = sources.resolve("${type.replace('.', '/')}.java")
        file.parentFile.mkdirs()
        file.writeText(
            """
            package ${type.substringBeforeLast('.')};
            public final class ChunkSessionMethods implements dev.chunkzero.runtime.SessionMethodProvider {
                public java.util.Collection<dev.chunkzero.runtime.SessionMethodBinding<?, ?>> methods() {
                    return java.util.List.of(${bindings.joinToString(",\n")});
                }
            }
            """.trimIndent() + "\n",
        )
        service.parentFile.mkdirs()
        service.writeText("$type\n")
    }
    val manifest = resources.resolve("META-INF/chunk/session-methods.json")
    manifest.parentFile.mkdirs()
    manifest.writeText(Gson().toJson(mapOf("version" to 1, "app" to app, "methods" to declarations)) + "\n")
}
