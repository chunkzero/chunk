package dev.chunkzero.gradle

import com.google.gson.Gson
import com.google.gson.JsonParser
import java.io.File

internal fun writeSessionConfigurations(
    app: String,
    providers: Map<String, String>,
    contract: File,
    lookup: (String) -> CompiledClass?,
    resources: File,
) {
    val metadata = JsonParser.parseString(contract.readText()).asJsonObject
    require(metadata["version"]?.asInt == 1) { "Unsupported session configuration metadata version" }
    val configurations =
        requireNotNull(metadata["configurations"]?.asJsonArray) { "Missing session configurations" }
            .map { it.asJsonObject }
    val declarations = configurations.filter { it["app"].asString == app }

    fun inherits(
        name: String,
        target: String,
        seen: MutableSet<String> = mutableSetOf(),
    ): Boolean =
        name == target || (seen.add(name) && lookup(name)?.parents.orEmpty().any { inherits(it, target, seen) })

    for (declaration in declarations) {
        val session = declaration["session"].asString
        val provider = requireNotNull(providers[session]) { "Unknown configured session: $app/$session" }
        require(inherits(provider.replace('.', '/'), declaration["binary_interface"].asString.replace('.', '/'))) {
            "Session $app/$session must implement ${declaration["interface"].asString} for its creation configuration"
        }
    }
    for ((session, provider) in providers) {
        val type = provider.replace('.', '/')
        val configured = inherits(type, "dev/chunkzero/runtime/ConfiguredSessionProvider")
        val declared = declarations.any { it["session"].asString == session }
        require(
            configured == declared,
        ) { "Session $app/$session creation configuration differs from its app declaration" }
        val foreign =
            configurations.any {
                (it["app"].asString != app || it["session"].asString != session) &&
                    inherits(type, it["binary_interface"].asString.replace('.', '/'))
            }
        require(!foreign) { "Session $app/$session implements creation configuration belonging to another session" }
    }
    val manifest = resources.resolve("META-INF/chunk/session-configurations.json")
    manifest.parentFile.mkdirs()
    manifest.writeText(
        Gson().toJson(
            mapOf(
                "version" to 1,
                "app" to app,
                "configurations" to
                    declarations.map {
                        it.deepCopy().apply { addProperty("provider", providers[it["session"].asString]) }
                    },
            ),
        ) + "\n",
    )
}
