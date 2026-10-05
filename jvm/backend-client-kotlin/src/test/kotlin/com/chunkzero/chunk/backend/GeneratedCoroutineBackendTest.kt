package com.chunkzero.chunk.backend

import chunk.sync.v1.CoreGrpc
import chunk.sync.v1.CoreOuterClass.CallRequest
import chunk.sync.v1.CoreOuterClass.CallResponse
import chunk.sync.v1.CoreOuterClass.Caller
import chunk.sync.v1.CoreOuterClass.Entry
import chunk.sync.v1.CoreOuterClass.Error
import chunk.sync.v1.CoreOuterClass.Position
import chunk.sync.v1.CoreOuterClass.SubscribeRequest
import chunk.sync.v1.CoreOuterClass.Update
import com.chunkzero.chunk.backend.api.PlayerId
import com.chunkzero.chunk.backend.api.SessionId
import com.chunkzero.chunk.backend.client.BackendSession
import com.chunkzero.chunk.backend.client.OperationId
import com.chunkzero.chunk.backend.client.QueryResult
import com.chunkzero.chunk.backend.client.SessionIdentity
import com.chunkzero.chunk.backend.client.WatchState
import com.chunkzero.chunk.generated.BackendTypes
import com.chunkzero.chunk.generated.CoroutineBackendClient
import com.google.protobuf.ByteString
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
                            .arguments
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
                    assertEquals(
                        Caller
                            .newBuilder()
                            .setSession("s1")
                            .setPlayer("trusted")
                            .build(),
                        fixture.calls[1].caller,
                    )

                    val pending = launch { playerBackend.shared.hang(null) }
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
                        assertEquals("queries", watch.request.topic)
                        assertEquals(
                            """{"0":{"function":"shared/profile/total","arguments":{}}}""",
                            watch.request.arguments.toStringUtf8(),
                        )
                        watch.response.onNext(update(7, value("3")).setSnapshot(true).build())
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
                        watch.response.onNext(update(8, failure("missing document")).build())
                        val failed = states.receive()
                        assertFalse(failed.stale())
                        assertEquals(8L, failed.snapshot().orElseThrow().revision())
                        val failure =
                            assertInstanceOf(QueryResult.Failure::class.java, failed.snapshot().orElseThrow().result())
                        assertEquals("missing document", failure.message())
                        watch.response.onNext(
                            Update
                                .newBuilder()
                                .setError(
                                    Error
                                        .newBuilder()
                                        .setCode(Error.Code.CODE_DENIED)
                                        .setMessage("watch forbidden"),
                                ).build(),
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
        entry: Entry.Builder,
    ): Update.Builder =
        Update
            .newBuilder()
            .setPosition(Position.newBuilder().setEpoch(1).setRevision(revision))
            .addUpserts(entry.setKey("0"))

    private fun value(json: String): Entry.Builder = Entry.newBuilder().setValue(ByteString.copyFromUtf8(json))

    private fun failure(message: String): Entry.Builder =
        Entry.newBuilder().setError(Error.newBuilder().setCode(Error.Code.CODE_APPLICATION).setMessage(message))

    private class Watch(
        val request: SubscribeRequest,
        val response: ServerCallStreamObserver<Update>,
        val cancelled: CompletableDeferred<Unit>,
    )

    private class Fixture :
        CoreGrpc.CoreImplBase(),
        AutoCloseable {
        val calls = CopyOnWriteArrayList<CallRequest>()
        val saved = ConcurrentHashMap<String, CallResponse>()
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
                BackendSession.overCore(
                    channel,
                    "test-credential-with-at-least-32-bytes",
                    "immutable-build",
                    SessionIdentity(SessionId("s1"), "duels", Optional.of(PlayerId("trusted"))),
                    scheduler,
                    Duration.ofSeconds(5),
                ),
                CoroutineScope(owner),
            )
        val client = CoroutineBackendClient(backend)

        override fun call(
            request: CallRequest,
            response: StreamObserver<CallResponse>,
        ) {
            calls.add(request)
            if (request.method == "shared/hang") {
                (response as ServerCallStreamObserver<CallResponse>).setOnCancelHandler {
                    callCancelled.complete(
                        Unit,
                    )
                }
                hanging.complete(Unit)
                return
            }
            val result =
                CallResponse
                    .newBuilder()
                    .setPosition(Position.newBuilder().setEpoch(1).setRevision(1))
                    .setResult(
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

        override fun subscribe(
            request: SubscribeRequest,
            response: StreamObserver<Update>,
        ) {
            val observer = response as ServerCallStreamObserver<Update>
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
