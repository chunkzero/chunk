package dev.chunkzero.backend

import chunk.v1.BackendGrpc
import chunk.v1.BackendOuterClass.BackendCall
import chunk.v1.BackendOuterClass.BackendResult
import chunk.v1.BackendOuterClass.BackendUpdate
import chunk.v1.BackendOuterClass.BackendWatch
import com.google.protobuf.ByteString
import io.grpc.Channel
import io.grpc.Metadata
import io.grpc.Status
import io.grpc.stub.ClientCallStreamObserver
import io.grpc.stub.ClientResponseObserver
import io.grpc.stub.MetadataUtils
import java.util.concurrent.CompletableFuture
import java.util.concurrent.ScheduledExecutorService
import java.util.concurrent.ScheduledFuture
import java.util.concurrent.TimeUnit
import java.util.function.Consumer

data class Query(
    val function: String,
    val arguments: ByteString,
)

/** All values belong to one snapshot. A failed stream retains values with stale=true. */
data class SubscriptionState(
    val stale: Boolean,
    val snapshot: BackendUpdate?,
)

/**
 * Bound to the session's immutable deployment. Channel and scheduler belong to its parent scope.
 * Callers encode JSON using their declared contracts. Mutations require a stable operation ID;
 * after a lost reply, recover by calling with that same ID and identical arguments.
 */
class BackendClient(
    channel: Channel,
    credential: String,
    private val environment: String,
    private val deployment: String,
    private val caller: ByteString,
    private val scheduler: ScheduledExecutorService,
) {
    private val stub =
        BackendGrpc.newStub(channel).withInterceptors(
            MetadataUtils.newAttachHeadersInterceptor(
                Metadata().apply {
                    put(Metadata.Key.of("authorization", Metadata.ASCII_STRING_MARSHALLER), "Bearer $credential")
                },
            ),
        )

    private fun request(
        query: Query,
        operation: String = "",
    ) = BackendCall
        .newBuilder()
        .setEnvironment(environment)
        .setDeployment(deployment)
        .setFunction(query.function)
        .setArgumentsJson(query.arguments)
        .setCallerJson(caller)
        .setOperationId(operation)
        .build()

    fun call(
        query: Query,
        operation: String,
    ): CompletableFuture<BackendResult> {
        val result = CompletableFuture<BackendResult>()
        stub.withDeadlineAfter(5, TimeUnit.SECONDS).call(
            request(query, operation),
            object : ClientResponseObserver<BackendCall, BackendResult> {
                override fun beforeStart(stream: ClientCallStreamObserver<BackendCall>) {
                    result.whenComplete { _, _ -> if (result.isCancelled) stream.cancel("scope closed", null) }
                }

                override fun onNext(value: BackendResult) {
                    result.complete(value)
                }

                override fun onError(error: Throwable) {
                    result.completeExceptionally(error)
                }

                override fun onCompleted() {
                    if (!result.isDone) result.completeExceptionally(IllegalStateException("missing backend result"))
                }
            },
        )
        return result
    }

    /** Close with the session scope. The serialized observer must return promptly. */
    fun subscribe(
        queries: List<Query>,
        observer: Consumer<SubscriptionState>,
    ): AutoCloseable {
        require(queries.isNotEmpty() && queries.size <= 16)
        return Subscription(
            BackendWatch.newBuilder().addAllQueries(queries.map(::request)).build(),
            observer,
        ).apply { start() }
    }

    private inner class Subscription(
        private val request: BackendWatch,
        private val observer: Consumer<SubscriptionState>,
    ) : AutoCloseable {
        private var closed = false
        private var generation = 0L
        private var snapshot: BackendUpdate? = null
        private var stream: ClientCallStreamObserver<BackendWatch>? = null
        private var retry: ScheduledFuture<*>? = null
        private var retryDelayMillis = 500L

        @Synchronized
        fun start() {
            observer.accept(SubscriptionState(stale = true, snapshot = null))
            connect()
        }

        @Synchronized
        private fun connect() {
            if (closed) return
            val attempt = ++generation
            stub.watch(
                request,
                object : ClientResponseObserver<BackendWatch, BackendUpdate> {
                    override fun beforeStart(call: ClientCallStreamObserver<BackendWatch>) {
                        synchronized(this@Subscription) {
                            if (closed || attempt != generation) call.cancel("scope closed", null) else stream = call
                        }
                    }

                    override fun onNext(value: BackendUpdate) {
                        synchronized(this@Subscription) {
                            if (closed || attempt != generation) return
                            snapshot = value
                            retryDelayMillis = 500L
                            observer.accept(SubscriptionState(stale = false, snapshot = value))
                        }
                    }

                    override fun onError(error: Throwable) {
                        failed(attempt, Status.fromThrowable(error))
                    }

                    override fun onCompleted() {
                        failed(attempt, Status.UNAVAILABLE)
                    }
                },
            )
        }

        @Synchronized
        private fun failed(
            attempt: Long,
            status: Status,
        ) {
            if (closed || generation != attempt) return
            ++generation
            stream = null
            observer.accept(SubscriptionState(stale = true, snapshot = snapshot))
            if (status.code in
                setOf(Status.Code.UNAVAILABLE, Status.Code.RESOURCE_EXHAUSTED, Status.Code.DEADLINE_EXCEEDED)
            ) {
                retry = scheduler.schedule(::connect, retryDelayMillis, TimeUnit.MILLISECONDS)
                retryDelayMillis = (retryDelayMillis * 2).coerceAtMost(5000L)
            }
        }

        @Synchronized
        override fun close() {
            if (closed) return
            closed = true
            ++generation
            retry?.cancel(false)
            stream?.cancel("session scope closed", null)
            stream = null
        }
    }
}
