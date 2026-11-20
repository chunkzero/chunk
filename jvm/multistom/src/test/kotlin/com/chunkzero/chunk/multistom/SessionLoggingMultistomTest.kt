package com.chunkzero.chunk.multistom

import chunk.sync.v1.Jvm.JvmSession
import com.chunkzero.chunk.multistom.event.SessionCreateEvent
import com.chunkzero.chunk.runtime.SessionLogging
import com.chunkzero.chunk.runtime.TestHosts
import net.minestom.server.ServerProcess
import org.junit.jupiter.api.AfterEach
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.BeforeEach
import org.junit.jupiter.api.Test
import java.util.concurrent.CompletableFuture
import java.util.function.Supplier

class SessionLoggingMultistomTest {
    private val ticks = TickExecutor()
    private lateinit var process: ServerProcess

    @BeforeEach
    fun start() {
        process = ServerProcess.create()
    }

    @AfterEach
    fun stop() {
        process.stop()
    }

    @Test
    fun `session hooks, onTick, scheduler tasks and event listeners bind the session id`() {
        val observed = mutableListOf<String>()
        process.exceptionManager().setExceptionHandler { observed.add("failure:${SessionLogging.current()}") }
        lateinit var scope: SessionScope
        val manager =
            SessionManager(
                process,
                ticks,
                mapOf(
                    "app/default" to
                        Supplier<Session> {
                            object : Session() {
                                override fun onCreate(created: SessionScope): CompletableFuture<Void> {
                                    scope = created
                                    observed.add("create:${SessionLogging.current()}")
                                    scope.createInstance()
                                    scope.scheduler
                                        .buildTask { observed.add("scheduler:${SessionLogging.current()}") }
                                        .schedule()
                                    return CompletableFuture.completedFuture(null)
                                }

                                override fun onFinish(): CompletableFuture<Void> {
                                    observed.add("finish:${SessionLogging.current()}")
                                    return CompletableFuture.completedFuture(null)
                                }
                            }
                        },
                ),
            )
        val host = TestHosts.detached(manager)
        val spec =
            JvmSession
                .newBuilder()
                .setSessionType("app/default")
                .setCapacity(4)
                .build()
        val created = TestHosts.create(host, "sess-42", spec)
        while (!created.isDone) ticks.flush()
        created.join()

        // Events reach the session's listeners from outside any session's context.
        scope.events.addListener(SessionCreateEvent::class.java) {
            observed.add("event:${SessionLogging.current()}")
            error("listener failed")
        }
        process.eventHandler().call(SessionCreateEvent(scope))
        assertNull(SessionLogging.current())

        val tickFuture = scope.onTick { observed.add("onTick:${SessionLogging.current()}") }
        ticks.flush()
        tickFuture.join()

        process.schedulerManager().processTick()
        assertNull(SessionLogging.current())

        val finished = TestHosts.finish(host, "sess-42", spec)
        while (!finished.isDone) ticks.flush()
        finished.join()

        assertNull(SessionLogging.current())
        assertEquals(
            listOf(
                "create:sess-42",
                "event:sess-42",
                "failure:sess-42",
                "onTick:sess-42",
                "scheduler:sess-42",
                "finish:sess-42",
            ),
            observed,
        )
    }
}
