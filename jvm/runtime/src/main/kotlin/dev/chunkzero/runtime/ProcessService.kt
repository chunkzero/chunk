package dev.chunkzero.runtime

import chunk.v1.ProcessControlGrpc
import chunk.v1.Supervision.ProcessIdentity
import chunk.v1.Supervision.ProcessInventory
import io.grpc.Status
import io.grpc.stub.StreamObserver
import java.util.concurrent.CountDownLatch
import java.util.concurrent.atomic.AtomicLong

internal class ProcessService(
    private val identity: ProcessIdentity,
    private val gameplay: GameplayService,
    private val ticks: AtomicLong,
    private val shutdown: CountDownLatch,
) : ProcessControlGrpc.ProcessControlImplBase() {
    override fun inventory(
        request: ProcessIdentity,
        response: StreamObserver<ProcessInventory>,
    ) {
        if (request != identity) {
            response.onError(Status.FAILED_PRECONDITION.withDescription("Stale process identity").asRuntimeException())
            return
        }
        response.onNext(
            ProcessInventory
                .newBuilder()
                .setIdentity(identity)
                .setTickCount(ticks.get())
                .addAllDeliveries(gameplay.deliveries())
                .addAllSessions(gameplay.sessions())
                .build(),
        )
        response.onCompleted()
    }

    override fun stopProcess(
        request: ProcessIdentity,
        response: StreamObserver<ProcessIdentity>,
    ) {
        if (request != identity) {
            response.onError(Status.FAILED_PRECONDITION.withDescription("Stale process identity").asRuntimeException())
            return
        }
        response.onNext(identity)
        response.onCompleted()
        Thread.startVirtualThread {
            Thread.sleep(100)
            shutdown.countDown()
        }
    }
}
