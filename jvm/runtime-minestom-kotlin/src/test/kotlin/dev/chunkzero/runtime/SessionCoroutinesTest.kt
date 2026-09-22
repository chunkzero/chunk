package dev.chunkzero.runtime

import chunk.v1.BackendGrpc
import chunk.v1.BackendOuterClass.BackendMutation
import chunk.v1.BackendOuterClass.BackendResult
import chunk.v1.BackendOuterClass.BackendUpdate
import chunk.v1.BackendOuterClass.BackendWatchGroup
import chunk.v1.Common.SessionRef
import chunk.v1.Supervision.SessionCommand
import chunk.v1.Supervision.SessionPhase
import com.google.protobuf.ByteString
import dev.chunkzero.backend.CoroutineBackend
import dev.chunkzero.backend.api.BackendValues
import dev.chunkzero.backend.api.JsonType
import dev.chunkzero.backend.api.MutationRef
import dev.chunkzero.backend.api.QueryRef
import dev.chunkzero.backend.api.SessionId
import dev.chunkzero.backend.client.BackendSession
import dev.chunkzero.backend.client.OperationId
import dev.chunkzero.backend.client.SessionIdentity
import io.grpc.ManagedChannelBuilder
import io.grpc.ServerBuilder
import io.grpc.stub.ServerCallStreamObserver
import io.grpc.stub.StreamObserver
import kotlinx.coroutines.flow.launchIn
import kotlinx.coroutines.flow.onEach
import net.minestom.server.ServerProcess
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import tools.jackson.core.type.TypeReference
import java.time.Duration
import java.util.Optional
import java.util.concurrent.CompletableFuture
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.function.Supplier

class SessionCoroutinesTest {
    private val nullType = JsonType.of(object : TypeReference<Void?>() {}, BackendValues::checkNull)
    private val integerType = JsonType.of(object : TypeReference<Long>() {}, BackendValues::checkInteger)

    @Test
    fun `tick resumptions and finishing await the result before disposing watches`() {
        val minestom = ServerProcess.create()
        val ticks = TickExecutor()
        val mutationStarted = CompletableFuture<Unit>()
        val result = CompletableFuture<Unit>()
        val watchClosed = CompletableFuture<Unit>()
        val server =
            ServerBuilder
                .forPort(0)
                .addService(
                    object : BackendGrpc.BackendImplBase() {
                        override fun mutate(
                            request: BackendMutation,
                            response: StreamObserver<BackendResult>,
                        ) {
                            mutationStarted.complete(Unit)
                            result.thenRun {
                                response.onNext(
                                    BackendResult
                                        .newBuilder()
                                        .setRevision(
                                            2,
                                        ).setResultJson(ByteString.copyFromUtf8("7"))
                                        .build(),
                                )
                                response.onCompleted()
                            }
                        }

                        override fun watchGroup(
                            request: BackendWatchGroup,
                            response: StreamObserver<BackendUpdate>,
                        ) {
                            (response as ServerCallStreamObserver<BackendUpdate>).setOnCancelHandler {
                                watchClosed.complete(
                                    Unit,
                                )
                            }
                            response.onNext(
                                BackendUpdate
                                    .newBuilder()
                                    .setRevision(
                                        1,
                                    ).addResultsJson(ByteString.copyFromUtf8("3"))
                                    .addErrors("")
                                    .build(),
                            )
                        }
                    },
                ).build()
                .start()
        val channel = ManagedChannelBuilder.forAddress("127.0.0.1", server.port).usePlaintext().build()
        val scheduler = Executors.newSingleThreadScheduledExecutor()
        var observed = false
        var committed = false
        val manager =
            SessionManager(
                minestom,
                ticks,
                mapOf(
                    "game" to
                        Supplier {
                            object : CoroutineSession() {
                                lateinit var backend: CoroutineBackend

                                override suspend fun create(scope: SessionScope) {
                                    scope.createInstance()
                                    backend =
                                        scope.coroutines.backend(
                                            BackendSession(
                                                channel,
                                                "test-credential-with-at-least-32-bytes",
                                                "local",
                                                "build",
                                                SessionIdentity(SessionId(scope.id), "duels", Optional.empty()),
                                                scheduler,
                                                Duration.ofSeconds(5),
                                            ),
                                        )
                                    backend
                                        .watch(
                                            QueryRef("read", nullType, integerType),
                                            null,
                                        ).onEach {
                                            ticks.checkThread()
                                            if (!it.stale()) observed = true
                                        }.launchIn(scope.coroutines)
                                }

                                override suspend fun finish() {
                                    val value =
                                        backend.mutate(
                                            MutationRef("finish", nullType, integerType),
                                            null,
                                            OperationId("final-result"),
                                        )
                                    ticks.checkThread()
                                    assertEquals(7L, value)
                                    committed = true
                                }
                            }
                        },
                    "flat" to
                        Supplier {
                            object : Session() {
                                override fun onCreate(scope: SessionScope): CompletableFuture<Void> {
                                    scope.createInstance()
                                    return CompletableFuture.completedFuture(null)
                                }
                            }
                        },
                ),
            )

        fun command(
            id: String,
            type: String,
        ) = SessionCommand
            .newBuilder()
            .setOperationId(
                id,
            ).setSession(SessionRef.newBuilder().setId(id))
            .setGeneration(1)
            .setSessionType(type)
            .setCapacity(2)
            .build()

        fun pump(until: () -> Boolean) {
            val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5)
            while (!until() && System.nanoTime() < deadline) {
                ticks.flush()
                Thread.sleep(1)
            }
            assertTrue(until())
        }
        try {
            val game = command("game", "game")
            val other = command("other", "flat")
            val created = manager.create(game)
            val independent = manager.create(other)
            pump { created.isDone && independent.isDone && observed }
            assertEquals(SessionPhase.SESSION_PHASE_READY, created.join().phase)
            val ending = manager.finish(game)
            pump { mutationStarted.isDone }
            assertFalse(ending.isDone)
            assertFalse(watchClosed.isDone)
            result.complete(Unit)
            pump { ending.isDone && watchClosed.isDone }
            assertEquals(SessionPhase.SESSION_PHASE_ENDED, ending.join().phase)
            assertTrue(committed)
            assertEquals(SessionPhase.SESSION_PHASE_READY, manager.get("other", 1).phase)
            val stopOther = manager.finish(other)
            pump { stopOther.isDone }
        } finally {
            result.complete(Unit)
            channel.shutdownNow().awaitTermination(2, TimeUnit.SECONDS)
            server.shutdownNow().awaitTermination(2, TimeUnit.SECONDS)
            scheduler.shutdownNow()
            scheduler.awaitTermination(2, TimeUnit.SECONDS)
            minestom.stop()
        }
    }
}
