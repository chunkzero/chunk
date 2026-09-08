package dev.chunkzero.backend

import chunk.v1.BackendGrpc
import chunk.v1.BackendOuterClass.BackendCall
import chunk.v1.BackendOuterClass.BackendResult
import chunk.v1.BackendOuterClass.BackendUpdate
import chunk.v1.BackendOuterClass.BackendWatch
import com.google.protobuf.ByteString
import io.grpc.Status
import io.grpc.netty.shaded.io.grpc.netty.NettyChannelBuilder
import io.grpc.netty.shaded.io.grpc.netty.NettyServerBuilder
import io.grpc.stub.ServerCallStreamObserver
import io.grpc.stub.StreamObserver
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import java.net.InetSocketAddress
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

class BackendClientTest {
    @Test
    fun `reconnect retains stale values then replaces the whole snapshot and closes with scope`() {
        val watches = LinkedBlockingQueue<StreamObserver<BackendUpdate>>()
        val cancelled = CountDownLatch(1)
        val server =
            NettyServerBuilder
                .forAddress(InetSocketAddress("127.0.0.1", 0))
                .addService(
                    object : BackendGrpc.BackendImplBase() {
                        override fun call(
                            request: BackendCall,
                            observer: StreamObserver<BackendResult>,
                        ) {
                            assertEquals("pinned", request.deployment)
                            assertEquals("same-operation", request.operationId)
                            observer.onNext(
                                BackendResult
                                    .newBuilder()
                                    .setRevision(7)
                                    .setResultJson(ByteString.copyFromUtf8("3"))
                                    .build(),
                            )
                            observer.onCompleted()
                        }

                        override fun watch(
                            request: BackendWatch,
                            observer: StreamObserver<BackendUpdate>,
                        ) {
                            assertEquals(listOf("pinned", "pinned"), request.queriesList.map { it.deployment })
                            (observer as ServerCallStreamObserver<BackendUpdate>).setOnCancelHandler {
                                cancelled
                                    .countDown()
                            }
                            watches.add(observer)
                        }
                    },
                ).build()
                .start()
        val channel = NettyChannelBuilder.forAddress("127.0.0.1", server.port).usePlaintext().build()
        val scheduler = Executors.newSingleThreadScheduledExecutor()
        try {
            val client =
                BackendClient(channel, "test-credential", "local", "pinned", ByteString.copyFromUtf8("{}"), scheduler)
            val query = Query("count", ByteString.copyFromUtf8("null"))
            assertEquals(7, client.call(query, "same-operation").get(3, TimeUnit.SECONDS).revision)
            val states = LinkedBlockingQueue<SubscriptionState>()
            client.subscribe(listOf(query, query), states::add).use {
                assertTrue(states.poll(3, TimeUnit.SECONDS).stale)
                val first = watches.poll(3, TimeUnit.SECONDS)
                first.onNext(snapshot(7, "3"))
                val fresh = states.poll(3, TimeUnit.SECONDS)
                assertFalse(fresh.stale)
                assertEquals(snapshot(7, "3"), fresh.snapshot)
                first.onError(Status.UNAVAILABLE.asRuntimeException())
                val stale = states.poll(3, TimeUnit.SECONDS)
                assertTrue(stale.stale)
                assertEquals(fresh.snapshot, stale.snapshot)
                watches.poll(3, TimeUnit.SECONDS).onNext(snapshot(9, "5"))
                val resumed = states.poll(3, TimeUnit.SECONDS)
                assertFalse(resumed.stale)
                assertEquals(snapshot(9, "5"), resumed.snapshot)
            }
            assertTrue(cancelled.await(3, TimeUnit.SECONDS))
        } finally {
            scheduler.shutdownNow()
            channel.shutdownNow().awaitTermination(3, TimeUnit.SECONDS)
            server.shutdownNow().awaitTermination(3, TimeUnit.SECONDS)
        }
    }

    private fun snapshot(
        revision: Long,
        value: String,
    ) = BackendUpdate
        .newBuilder()
        .setRevision(revision)
        .addAllResultsJson(listOf(ByteString.copyFromUtf8(value), ByteString.copyFromUtf8(value)))
        .build()
}
