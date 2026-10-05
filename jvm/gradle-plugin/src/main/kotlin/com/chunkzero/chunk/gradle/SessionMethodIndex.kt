package com.chunkzero.chunk.gradle

import com.google.gson.Gson
import com.google.gson.JsonParser
import java.io.File

internal fun writeSessionMethods(
    app: String,
    providers: Map<String, String>,
    contract: File,
    lookup: ClassLookup,
    resources: File,
    sources: File,
) {
    val metadata = JsonParser.parseString(contract.readText()).asJsonObject
    require(metadata["version"]?.asInt == 1) { "Unsupported session method metadata version" }
    val methods =
        requireNotNull(metadata["methods"]?.asJsonArray) { "Missing session methods" }
            .map { it.asJsonObject }
    val declarations = methods.filter { it["app"].asString == app }

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
            creates(type, lookup.inherits(type, "com/chunkzero/chunk/runtime/ConfiguredSessionProvider"))
        }
    val bindings =
        declarations.map { method ->
            val session = method["session"].asString
            val name = method["name"].asString
            val implementation = implementations[session]
            require(
                implementation != null && lookup(implementation)?.publicConcrete == true &&
                    lookup.inherits(implementation, "com/chunkzero/chunk/runtime/Session"),
            ) {
                "Session $app/$session method $name requires a provider creation method with a public concrete Session return type"
            }
            val contractInterface = method["binary_interface"].asString.replace('.', '/')
            require(lookup.inherits(implementation, contractInterface)) {
                "Session $app/$session must implement ${method["interface"].asString} for method $name"
            }
            val type = method["interface"].asString
            "new com.chunkzero.chunk.runtime.SessionMethodBinding<>($type.REF, " +
                "(session, args) -> (($type) session).${method["function"].asString}(args))"
        }
    for ((session, implementation) in implementations) {
        if (implementation == null) continue
        require(!lookup.implementsForeign(implementation, app, session, methods)) {
            "Session $app/$session implements a method belonging to another app or session"
        }
    }
    sources.deleteRecursively()
    sources.mkdirs()
    val service = resources.resolve("META-INF/services/com.chunkzero.chunk.runtime.SessionMethodProvider")
    service.delete()
    if (bindings.isNotEmpty()) {
        val type = "com.chunkzero.chunk.generated.sessions.a${app.toByteArray().joinToString(
            "",
        ) { "%02x".format(it) }}.ChunkSessionMethods"
        val file = sources.resolve("${type.replace('.', '/')}.java")
        file.parentFile.mkdirs()
        file.writeText(
            """
            package ${type.substringBeforeLast('.')};
            public final class ChunkSessionMethods implements com.chunkzero.chunk.runtime.SessionMethodProvider {
                public java.util.Collection<com.chunkzero.chunk.runtime.SessionMethodBinding<?, ?>> methods() {
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
