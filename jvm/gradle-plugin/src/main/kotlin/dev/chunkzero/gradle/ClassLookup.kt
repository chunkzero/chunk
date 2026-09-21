package dev.chunkzero.gradle

import com.google.gson.JsonObject

internal typealias ClassLookup = (String) -> CompiledClass?

internal fun ClassLookup.inherits(
    name: String,
    target: String,
    seen: MutableSet<String> = mutableSetOf(),
): Boolean = name == target || (seen.add(name) && this(name)?.parents.orEmpty().any { inherits(it, target, seen) })

/** Whether [type] implements a contract declared for a session other than [app]/[session]. */
internal fun ClassLookup.implementsForeign(
    type: String,
    app: String,
    session: String,
    declarations: List<JsonObject>,
): Boolean =
    declarations.any {
        (it["app"].asString != app || it["session"].asString != session) &&
            inherits(type, it["binary_interface"].asString.replace('.', '/'))
    }
