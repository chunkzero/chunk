package dev.chunkzero.backend

import chunk.v1.BackendGrpc
import chunk.v1.BackendOuterClass.BackendCall
import chunk.v1.BackendOuterClass.BackendResult
import chunk.v1.BackendOuterClass.BackendUpdate
import chunk.v1.BackendOuterClass.BackendWatch
import com.google.protobuf.ByteString
import dev.chunkzero.backend.api.Codecs
import dev.chunkzero.backend.api.NullValue
import dev.chunkzero.backend.api.PlayerId
import dev.chunkzero.backend.api.SessionId
import dev.chunkzero.backend.client.BackendSession
import dev.chunkzero.backend.client.OperationId
import dev.chunkzero.backend.client.QueryResult
import dev.chunkzero.backend.client.SessionIdentity
import dev.chunkzero.backend.client.WatchState
import dev.chunkzero.generated.BackendTypes
import dev.chunkzero.generated.CoroutineBackendClient
import io.grpc.ManagedChannelBuilder
import io.grpc.ServerBuilder
import io.grpc.Status
import io.grpc.stub.ServerCallStreamObserver
import io.grpc.stub.StreamObserver
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertInstanceOf
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import java.time.Duration
import java.util.Optional
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit

class GeneratedCoroutineBackendTest {
    @Test
    fun `generated suspensions preserve retry identity and cancel only their own call`() =
        runBlocking {
            withTimeout(5_000) {
                Fixture().use { fixture ->
                    val playerBackend = fixture.client
                    assertEquals(3L, playerBackend.shared.profile.total())
                    assertEquals(
                        "{}",
                        fixture.calls
                            .single()
                            .argumentsJson
                            .toStringUtf8(),
                    )
                    val operation = OperationId("stable-reward")
                    val error =
                        runCatching {
                            playerBackend.shared.profile.reward(
                                operation = operation,
                            )
                        }.exceptionOrNull()
                    assertEquals(Status.Code.UNAVAILABLE, Status.fromThrowable(requireNotNull(error)).code)
                    val result =
                        playerBackend.shared.profile.reward(
                            BackendTypes.Shared.Profile.RewardArgs(),
                            operation = operation,
                        )
                    assertTrue(result.ok())
                    assertEquals(SessionId("s1"), result.session())
                    assertEquals(1, fixture.saved.size)
                    assertEquals(fixture.calls[1], fixture.calls[2])
                    assertEquals(operation.value(), fixture.calls[1].operationId)
                    val caller = Codecs.parse(fixture.calls[1].callerJson.toStringUtf8()).asJsonObject
                    assertEquals("duels", caller.get("app").asString)
                    assertEquals("trusted", caller.get("player").asString)

                    val pending = launch { playerBackend.shared.hang(NullValue.INSTANCE) }
                    fixture.hanging.await()
                    pending.cancelAndJoin()
                    fixture.callCancelled.await()
                    assertTrue(fixture.owner.isActive)
                    assertEquals(3L, playerBackend.shared.profile.total())
                }
            }
        }

    @Test
    fun `generated flows retain full snapshots errors and cancellation`() =
        runBlocking {
            withTimeout(5_000) {
                Fixture().use { fixture ->
                    val states = Channel<WatchState<Long>>(Channel.UNLIMITED)
                    val collector =
                        launch {
                            fixture.client.shared.profile
                                .watchTotal()
                                .collect { states.send(it) }
                        }
                    try {
                        val initial = states.receive()
                        assertTrue(initial.stale())
                        assertTrue(initial.snapshot().isEmpty)
                        val watch = fixture.watches.receive()
                        assertEquals("shared/profile/total", watch.request.getQueries(0).function)
                        assertEquals(
                            "{}",
                            watch.request
                                .getQueries(0)
                                .argumentsJson
                                .toStringUtf8(),
                        )
                        watch.response.onNext(update(7, "3"))
                        val fresh = states.receive()
                        assertFalse(fresh.stale())
                        assertTrue(fresh.error().isEmpty)
                        assertEquals(7L, fresh.snapshot().orElseThrow().revision())
                        assertEquals(
                            3L,
                            fresh
                                .snapshot()
                                .orElseThrow()
                                .result()
                                .valueOrThrow(),
                        )
                        watch.response.onNext(update(8, "", "missing document"))
                        val failed = states.receive()
                        assertFalse(failed.stale())
                        assertEquals(8L, failed.snapshot().orElseThrow().revision())
                        val failure =
                            assertInstanceOf(QueryResult.Failure::class.java, failed.snapshot().orElseThrow().result())
                        assertEquals("missing document", failure.message())
                        watch.response.onError(
                            Status.PERMISSION_DENIED.withDescription("watch forbidden").asRuntimeException(),
                        )
                        val stopped = states.receive()
                        assertTrue(stopped.stale())
                        assertEquals(failed.snapshot(), stopped.snapshot())
                        assertTrue(stopped.error().orElseThrow().contains("watch forbidden"))
                    } finally {
                        collector.cancelAndJoin()
                        states.close()
                    }
                    val next =
                        launch {
                            fixture.client.shared.profile
                                .watchTotal(BackendTypes.Shared.Profile.TotalArgs())
                                .collect {}
                        }
                    val watch = fixture.watches.receive()
                    next.cancelAndJoin()
                    watch.cancelled.await()
                    assertTrue(fixture.owner.isActive)
                }
            }
        }

    private fun update(
        revision: Long,
        result: String,
        error: String = "",
    ): BackendUpdate =
        BackendUpdate
            .newBuilder()
            .setRevision(revision)
            .addResultsJson(ByteString.copyFromUtf8(result))
            .addErrors(error)
            .build()

    private class Watch(
        val request: BackendWatch,
        val response: ServerCallStreamObserver<BackendUpdate>,
        val cancelled: CompletableDeferred<Unit>,
    )

    private class Fixture :
        BackendGrpc.BackendImplBase(),
        AutoCloseable {
        val calls = CopyOnWriteArrayList<BackendCall>()
        val saved = ConcurrentHashMap<String, BackendResult>()
        val hanging = CompletableDeferred<Unit>()
        val callCancelled = CompletableDeferred<Unit>()
        val watches = Channel<Watch>(Channel.UNLIMITED)
        val owner = SupervisorJob()
        private val scheduler = Executors.newSingleThreadScheduledExecutor()
        private val server =
            ServerBuilder
                .forPort(0)
                .addService(this)
                .build()
                .start()
        private val channel = ManagedChannelBuilder.forAddress("127.0.0.1", server.port).usePlaintext().build()
        private val backend =
            CoroutineBackend(
                BackendSession(
                    channel,
                    "test-credential-with-at-least-32-bytes",
                    "local",
                    "immutable-build",
                    SessionIdentity(SessionId("s1"), "duels", Optional.of(PlayerId("trusted"))),
                    scheduler,
                    Duration.ofSeconds(5),
                ),
                CoroutineScope(owner),
            )
        val client = CoroutineBackendClient(backend)

        override fun call(
            request: BackendCall,
            response: StreamObserver<BackendResult>,
        ) {
            calls.add(request)
            if (request.function == "shared/hang") {
                (response as ServerCallStreamObserver<BackendResult>).setOnCancelHandler {
                    callCancelled.complete(
                        Unit,
                    )
                }
                hanging.complete(Unit)
                return
            }
            val result =
                BackendResult
                    .newBuilder()
                    .setRevision(1)
                    .setResultJson(
                        ByteString.copyFromUtf8(
                            if (request.operationId.isEmpty()) "3" else """{"ok":true,"session":"s1"}""",
                        ),
                    ).build()
            if (request.operationId.isNotEmpty() && saved.putIfAbsent(request.operationId, result) == null) {
                response.onError(Status.UNAVAILABLE.asRuntimeException())
            } else {
                response.onNext(result)
                response.onCompleted()
            }
        }

        override fun watch(
            request: BackendWatch,
            response: StreamObserver<BackendUpdate>,
        ) {
            val observer = response as ServerCallStreamObserver<BackendUpdate>
            val cancelled = CompletableDeferred<Unit>()
            observer.setOnCancelHandler { cancelled.complete(Unit) }
            watches.trySend(Watch(request, observer, cancelled)).getOrThrow()
        }

        override fun close() {
            backend.close()
            owner.cancel()
            channel.shutdownNow().awaitTermination(2, TimeUnit.SECONDS)
            server.shutdownNow().awaitTermination(2, TimeUnit.SECONDS)
            scheduler.shutdownNow()
            scheduler.awaitTermination(2, TimeUnit.SECONDS)
            watches.close()
        }
    }
}
