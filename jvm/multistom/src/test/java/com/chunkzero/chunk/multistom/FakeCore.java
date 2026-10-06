package com.chunkzero.chunk.multistom;

import static org.junit.jupiter.api.Assertions.fail;

import chunk.sync.v1.CoreGrpc;
import chunk.sync.v1.CoreOuterClass.CallRequest;
import chunk.sync.v1.CoreOuterClass.CallResponse;
import chunk.sync.v1.CoreOuterClass.Entry;
import chunk.sync.v1.CoreOuterClass.Error;
import chunk.sync.v1.CoreOuterClass.SubscribeRequest;
import chunk.sync.v1.CoreOuterClass.Update;
import chunk.sync.v1.Jvm.JvmDeliveryPhase;
import chunk.sync.v1.Jvm.JvmDeliveryStatus;
import chunk.sync.v1.Jvm.JvmHealth;
import chunk.sync.v1.Jvm.JvmMethodResult;
import chunk.sync.v1.Jvm.JvmRegistered;
import chunk.sync.v1.Jvm.JvmRegistration;
import chunk.sync.v1.Jvm.JvmReport;

import com.chunkzero.chunk.runtime.control.CoreLink;
import com.chunkzero.chunk.runtime.control.ProcessState;
import com.google.protobuf.ByteString;
import com.google.protobuf.InvalidProtocolBufferException;
import com.google.protobuf.Message;

import io.grpc.Server;
import io.grpc.ServerBuilder;
import io.grpc.stub.StreamObserver;

import java.io.IOException;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;

/**
 * A core serving one JVM's {@code jvm/<host>} topic to a {@link CoreLink}. Like core, it records
 * the deliveries the JVM reports and removes each once it is CLOSED, and records each method's
 * first result and then removes the method.
 */
public final class FakeCore extends CoreGrpc.CoreImplBase implements AutoCloseable {
    private final Server server;
    private final AtomicReference<CoreLink> link = new AtomicReference<>();
    private final Map<String, ByteString> entries = new TreeMap<>();
    private final Map<String, List<JvmDeliveryPhase>> phases = new ConcurrentHashMap<>();
    private final Map<String, JvmDeliveryStatus> deliveries = new ConcurrentHashMap<>();
    private final Map<String, JvmMethodResult> results = new ConcurrentHashMap<>();
    private StreamObserver<Update> topic;
    private String stream = "";

    FakeCore() throws IOException {
        server = ServerBuilder.forPort(0).addService(this).build().start();
    }

    /** Links {@code state} to this core, as a ready JVM would. */
    void connect(ProcessState state) {
        var connected =
                new CoreLink(
                        "http://127.0.0.1:" + server.getPort(),
                        "fake-core-credential-with-at-least-32-bytes",
                        JvmRegistration.getDefaultInstance(),
                        state,
                        JvmHealth::getDefaultInstance,
                        () -> {},
                        error -> {});
        link.set(connected);
        connected.start();
    }

    /** Wakes the link to report, as the engine's tick does. */
    public void wake() {
        var current = link.get();
        if (current != null) current.wake();
    }

    public void methodResult(String operation, JvmMethodResult result) {
        link.get().methodResult(operation, result);
    }

    synchronized void put(String key, Message value) {
        entries.put(key, value.toByteString());
        publish();
    }

    synchronized void remove(String key) {
        entries.remove(key);
        publish();
    }

    /** Waits for delivery {@code operation} to be reported in {@code phase}. */
    JvmDeliveryStatus await(String operation, JvmDeliveryPhase phase) throws InterruptedException {
        var deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
        while (System.nanoTime() < deadline) {
            var status = deliveries.get(operation);
            if (status != null && status.getPhase() == phase) return status;
            Thread.sleep(10);
        }
        return fail(operation + " was never " + phase + ", only " + phases(operation));
    }

    /** The phases reported for delivery {@code operation}, in order, without repeats. */
    synchronized List<JvmDeliveryPhase> phases(String operation) {
        return List.copyOf(phases.getOrDefault(operation, List.of()));
    }

    /** Runs {@code tick} until method {@code operation} has a result, and returns it. */
    JvmMethodResult result(String operation, Runnable tick) throws InterruptedException {
        var deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (System.nanoTime() < deadline) {
            tick.run();
            var result = results.get(operation);
            if (result != null) return result;
            Thread.sleep(10);
        }
        return fail("no result for " + operation);
    }

    @Override
    public synchronized void call(CallRequest request, StreamObserver<CallResponse> response) {
        var result = CallResponse.newBuilder().setResult(ByteString.EMPTY);
        try {
            switch (request.getMethod()) {
                case "chunk:register" ->
                        result.setResult(
                                JvmRegistered.newBuilder().setHost("host").build().toByteString());
                case "chunk:report" -> {
                    if (!request.getStream().equals(stream)) result.setError(stopped());
                    else report(JvmReport.parseFrom(request.getArguments()));
                }
                case "chunk:method_result" -> {
                    if (!request.getStream().equals(stream)) result.setError(stopped());
                    else {
                        results.putIfAbsent(
                                request.getOperationId(),
                                JvmMethodResult.parseFrom(request.getArguments()));
                        remove("method/" + request.getOperationId());
                    }
                }
                default -> result.setError(Error.newBuilder().setCode(Error.Code.CODE_INVALID));
            }
        } catch (InvalidProtocolBufferException error) {
            result.setError(Error.newBuilder().setCode(Error.Code.CODE_INVALID));
        }
        response.onNext(result.build());
        response.onCompleted();
    }

    private void report(JvmReport report) {
        for (var delivery : report.getDeliveriesList()) {
            var operation = delivery.getOperationId();
            deliveries.put(operation, delivery);
            var seen = phases.computeIfAbsent(operation, ignored -> new ArrayList<>());
            if (seen.isEmpty() || seen.getLast() != delivery.getPhase())
                seen.add(delivery.getPhase());
            if (delivery.getPhase() == JvmDeliveryPhase.JVM_DELIVERY_PHASE_CLOSED)
                remove("delivery/" + operation);
        }
    }

    @Override
    public synchronized void subscribe(SubscribeRequest request, StreamObserver<Update> response) {
        topic = response;
        stream = "stream-" + System.nanoTime();
        response.onNext(snapshot().setStream(stream).build());
    }

    private void publish() {
        if (topic != null) topic.onNext(snapshot().build());
    }

    private Update.Builder snapshot() {
        var update = Update.newBuilder().setSnapshot(true);
        entries.forEach(
                (key, value) -> update.addUpserts(Entry.newBuilder().setKey(key).setValue(value)));
        return update;
    }

    private static Error stopped() {
        return Error.newBuilder().setCode(Error.Code.CODE_STOPPED).build();
    }

    @Override
    public void close() {
        var current = link.get();
        if (current != null) current.close();
        server.shutdownNow();
        try {
            server.awaitTermination(3, TimeUnit.SECONDS);
        } catch (InterruptedException ignored) {
            Thread.currentThread().interrupt();
        }
    }
}
