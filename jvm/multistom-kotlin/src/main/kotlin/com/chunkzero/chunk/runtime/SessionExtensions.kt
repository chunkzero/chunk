package com.chunkzero.chunk.runtime

import net.minestom.server.entity.Player
import net.minestom.server.timer.Task
import kotlin.time.Duration
import kotlin.time.toJavaDuration

/** Returns one session-owned resource per erased class; access and creation require the tick thread. */
inline fun <reified T : AutoCloseable> SessionScope.resource(crossinline factory: () -> T): T =
    resource(T::class.java) { factory() }

/** Registers an existing task on the tick thread for cancellation when the session is disposed. */
fun SessionScope.own(task: Task): Task {
    own(AutoCloseable(task::cancel))
    return task
}

/** Registers an existing task on the tick thread for cancellation when this admitted player leaves. */
fun SessionScope.own(
    player: Player,
    task: Task,
): Task {
    own(player, AutoCloseable(task::cancel))
    return task
}

/** Schedules a session-owned task with a positive, finite Kotlin duration. */
fun SessionScope.repeatEvery(
    interval: Duration,
    action: () -> Unit,
): AutoCloseable {
    require(interval.isFinite()) { "Interval must be finite" }
    return repeatEvery(interval.toJavaDuration(), action)
}
