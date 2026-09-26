package dev.chunkzero.runtime.control;

import chunk.v1.Supervision.DeliveryInventory;
import chunk.v1.Supervision.DesiredSessions;
import chunk.v1.Supervision.ProcessIdentity;
import chunk.v1.Supervision.ProcessReport;
import chunk.v1.Supervision.SessionInventory;
import chunk.v1.SupervisorGrpc;

import io.grpc.ManagedChannel;
import io.grpc.Metadata;
import io.grpc.stub.MetadataUtils;
import io.grpc.stub.StreamObserver;

import org.jetbrains.annotations.Nullable;

import java.util.HashMap;
import java.util.HashSet;
import java.util.Map;

/**
 * The process's stream to control. Control sends the sessions to run or end; the process reports
 * everything it holds on the first flush after the stream opens, then only what changed.
 */
final class ProcessSync {
    private final ManagedChannel channel;
    private final String token;
    private final ProcessIdentity identity;
    private final ProcessState state;
    private final Map<String, SessionInventory> sessions = new HashMap<>();
    private final Map<String, DeliveryInventory> deliveries = new HashMap<>();
    private @Nullable Stream stream;

    ProcessSync(
            ManagedChannel channel, String token, ProcessIdentity identity, ProcessState state) {
        this.channel = channel;
        this.token = token;
        this.identity = identity;
        this.state = state;
    }

    /** Opens a stream unless one is open. */
    synchronized void open() {
        if (stream != null) return;
        var metadata = new Metadata();
        metadata.put(
                Metadata.Key.of("authorization", Metadata.ASCII_STRING_MARSHALLER),
                "Bearer " + token);
        var opened = new Stream();
        opened.reports =
                SupervisorGrpc.newStub(channel)
                        .withInterceptors(MetadataUtils.newAttachHeadersInterceptor(metadata))
                        .sync(opened);
        stream = opened;
        sessions.clear();
        deliveries.clear();
    }

    /** Reports what changed since the last report, or everything on a new stream. */
    synchronized void flush() {
        if (stream == null) return;
        var report = changes();
        if (!stream.reported || report.getSessionsCount() > 0 || report.getDeliveriesCount() > 0)
            stream.reports.onNext(report.build());
        stream.reported = true;
    }

    private ProcessReport.Builder changes() {
        var inventory = state.inventory();
        var report = ProcessReport.newBuilder().setIdentity(identity);
        var sessionIds = new HashSet<String>();
        for (var session : inventory.getSessionsList()) {
            sessionIds.add(session.getSession().getId());
            if (!session.equals(sessions.put(session.getSession().getId(), session)))
                report.addSessions(session);
        }
        var operationIds = new HashSet<String>();
        for (var delivery : inventory.getDeliveriesList()) {
            operationIds.add(delivery.getDelivery().getOperationId());
            if (!delivery.equals(deliveries.put(delivery.getDelivery().getOperationId(), delivery)))
                report.addDeliveries(delivery);
        }
        sessions.keySet().retainAll(sessionIds);
        deliveries.keySet().retainAll(operationIds);
        return report;
    }

    private synchronized void closed(Stream closed) {
        if (stream == closed) stream = null;
    }

    private final class Stream implements StreamObserver<DesiredSessions> {
        private StreamObserver<ProcessReport> reports;
        private boolean reported;

        @Override
        public void onNext(DesiredSessions desired) {
            state.apply(desired);
        }

        @Override
        public void onError(Throwable error) {
            closed(this);
        }

        @Override
        public void onCompleted() {
            closed(this);
        }
    }
}
