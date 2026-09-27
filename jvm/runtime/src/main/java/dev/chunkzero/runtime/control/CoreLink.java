package dev.chunkzero.runtime.control;

import chunk.sync.v1.CoreGrpc;
import chunk.sync.v1.CoreOuterClass.CallRequest;
import chunk.sync.v1.CoreOuterClass.CallResponse;
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
import chunk.sync.v1.Jvm.JvmSessionStatus;

import com.google.protobuf.ByteString;
import com.google.protobuf.InvalidProtocolBufferException;

import io.grpc.Context;
import io.grpc.ManagedChannel;
import io.grpc.Metadata;
import io.grpc.StatusRuntimeException;
import io.grpc.stub.MetadataUtils;
import io.grpc.stub.StreamObserver;

import org.jetbrains.annotations.ApiStatus;
import org.jetbrains.annotations.Nullable;

import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import java.util.concurrent.locks.LockSupport;
import java.util.function.Supplier;

/**
 * The JVM's link to core. It registers with {@code chunk:register}, follows its {@code jvm/<host>}
 * topic, and reports on each stream with {@code chunk:report}: everything it holds first, then what
 * changed and its health at least every few seconds. Once a stream ends or a call on it fails, it
 * registers and subscribes again. The topic's updates wait in a latest-value slot for the link's
 * own thread, which applies them and makes every call, on a channel of its own.
 */
@ApiStatus.Internal
public final class CoreLink implements AutoCloseable {
    private static final System.Logger LOG = System.getLogger(CoreLink.class.getName());
    private static final long HEALTH_NANOS = TimeUnit.SECONDS.toNanos(3);
    private static final long MIN_BACKOFF_MILLIS = 250;
    private static final long MAX_BACKOFF_MILLIS = 5000;

    private final ManagedChannel channel;
    private final CoreGrpc.CoreBlockingStub calls;
    private final CoreGrpc.CoreStub topics;
    private final JvmRegistration registration;
    private final ProcessState state;
    private final Supplier<JvmHealth> health;
    private final Runnable stop;
    private final Map<String, JvmMethodResult> results = new ConcurrentHashMap<>();
    private volatile boolean closed;
    private volatile @Nullable Thread worker;
    private String host = "";

    /**
     * {@code stop} runs once core asks the JVM to stop, or once core refuses its registration for
     * good.
     */
    public CoreLink(
            String endpoint,
            String credential,
            JvmRegistration registration,
            ProcessState state,
            Supplier<JvmHealth> health,
            Runnable stop) {
        this.registration = registration;
        this.state = state;
        this.health = health;
        this.stop = stop;
        var metadata = new Metadata();
        metadata.put(
                Metadata.Key.of("authorization", Metadata.ASCII_STRING_MARSHALLER),
                "Bearer " + credential);
        var authorization = MetadataUtils.newAttachHeadersInterceptor(metadata);
        channel = CoreChannel.open(endpoint);
        calls = CoreGrpc.newBlockingStub(channel).withInterceptors(authorization);
        topics = CoreGrpc.newStub(channel).withInterceptors(authorization);
    }

    /**
     * Registers, retrying while core is unavailable, then follows the topic in the background.
     *
     * @throws IllegalStateException if core refuses the registration or the link closes first
     */
    public void start() {
        long backoff = MIN_BACKOFF_MILLIS;
        while (true) {
            if (closed) throw new IllegalStateException("Core link closed");
            try {
                register();
                break;
            } catch (Refused error) {
                throw new IllegalStateException("Core refused the registration", error);
            } catch (RuntimeException error) {
                pause(backoff);
                backoff = Math.min(backoff * 2, MAX_BACKOFF_MILLIS);
            }
        }
        worker = Thread.ofVirtual().name("chunk-core-link").start(this::run);
    }

    /** Sends a session method's result to core, until core records it or stops wanting it. */
    public void methodResult(String operation, JvmMethodResult result) {
        results.put(operation, result);
        wake();
    }

    /** Wakes the link to report what changed. */
    public void wake() {
        var current = worker;
        if (current != null) LockSupport.unpark(current);
    }

    @Override
    public void close() {
        closed = true;
        channel.shutdownNow();
        var current = worker;
        if (current == null || current == Thread.currentThread()) return;
        LockSupport.unpark(current);
        try {
            current.join(TimeUnit.SECONDS.toMillis(3));
        } catch (InterruptedException ignored) {
            Thread.currentThread().interrupt();
        }
    }

    /** Follows the topic, registering again before every stream but the first. */
    private void run() {
        long backoff = MIN_BACKOFF_MILLIS;
        boolean registered = true;
        while (!closed) {
            try {
                if (!registered) register();
                registered = false;
                if (follow()) backoff = MIN_BACKOFF_MILLIS;
            } catch (Refused error) {
                LOG.log(System.Logger.Level.ERROR, "Core refused the JVM's registration", error);
                stop.run();
                return;
            } catch (RuntimeException ignored) {
                // Core is unreachable or the stream broke; register and subscribe again.
            }
            pause(backoff);
            backoff = Math.min(backoff * 2, MAX_BACKOFF_MILLIS);
        }
    }

    private void register() {
        var response =
                call(
                        CallRequest.newBuilder()
                                .setMethod("chunk:register")
                                .setArguments(registration.toByteString()));
        if (response.hasError()) {
            var code = response.getError().getCode();
            if (code == Error.Code.CODE_UNAVAILABLE || code == Error.Code.CODE_OVERLOADED)
                throw new IllegalStateException(
                        "Core can't register the JVM now: " + response.getError().getMessage());
            throw new Refused(response.getError());
        }
        try {
            host = JvmRegistered.parseFrom(response.getResult()).getHost();
        } catch (InvalidProtocolBufferException error) {
            throw new IllegalStateException("Invalid registration result", error);
        }
    }

    /**
     * Follows one stream of the topic until it ends or a call on it fails, and returns whether core
     * accepted a complete report on it.
     */
    private boolean follow() {
        var stream = new Stream(Thread.currentThread());
        var context = Context.ROOT.withCancellation();
        try {
            context.run(
                    () ->
                            topics.subscribe(
                                    SubscribeRequest.newBuilder().setTopic("jvm/" + host).build(),
                                    stream));
            Reported reported = null;
            long healthDue = 0;
            while (!closed && !stream.ended) {
                var entries = stream.latest.getAndSet(null);
                if (entries != null) apply(entries);
                var id = stream.id;
                if (id != null) {
                    var inventory = state.inventory();
                    var report = changes(inventory, reported);
                    if (reported == null
                            || report.getSessionsCount() > 0
                            || report.getDeliveriesCount() > 0
                            || System.nanoTime() - healthDue >= 0) {
                        report.setHealth(health.get());
                        if (!send(id, "chunk:report", "", report.build().toByteString()))
                            return reported != null;
                        reported = new Reported(inventory);
                        healthDue = System.nanoTime() + HEALTH_NANOS;
                    }
                    for (var result : List.copyOf(results.entrySet())) {
                        if (!send(
                                id,
                                "chunk:method_result",
                                result.getKey(),
                                result.getValue().toByteString())) return true;
                        results.remove(result.getKey(), result.getValue());
                    }
                }
                LockSupport.parkNanos(id == null ? HEALTH_NANOS : healthDue - System.nanoTime());
            }
            return reported != null;
        } finally {
            context.cancel(null);
        }
    }

    private void apply(Map<String, ByteString> entries) {
        results.keySet().removeIf(operation -> !entries.containsKey("method/" + operation));
        state.apply(entries);
        if (entries.containsKey("stop")) stop.run();
    }

    /**
     * Everything in {@code inventory} unless {@code reported} is set, and otherwise what changed
     * since it: a delivery that is no longer held is closed.
     */
    private static JvmReport.Builder changes(JvmReport inventory, @Nullable Reported reported) {
        var report = JvmReport.newBuilder().setComplete(reported == null);
        for (var session : inventory.getSessionsList()) {
            if (reported == null || !session.equals(reported.sessions.get(session.getId())))
                report.addSessions(session);
        }
        var held = new HashMap<String, JvmDeliveryStatus>();
        for (var delivery : inventory.getDeliveriesList()) {
            held.put(delivery.getOperationId(), delivery);
            if (reported == null
                    || !delivery.equals(reported.deliveries.get(delivery.getOperationId())))
                report.addDeliveries(delivery);
        }
        if (reported != null) {
            for (var delivery : reported.deliveries.values()) {
                if (!held.containsKey(delivery.getOperationId())
                        && delivery.getPhase() != JvmDeliveryPhase.JVM_DELIVERY_PHASE_CLOSED)
                    report.addDeliveries(
                            delivery.toBuilder()
                                    .setPhase(JvmDeliveryPhase.JVM_DELIVERY_PHASE_CLOSED)
                                    .clearCapability());
            }
        }
        return report;
    }

    /**
     * Calls {@code method} on stream {@code id}, and returns whether the stream may go on: core
     * applied the call, or refused it for good.
     */
    private boolean send(String id, String method, String operation, ByteString arguments) {
        CallResponse response;
        try {
            response =
                    call(
                            CallRequest.newBuilder()
                                    .setOperationId(operation)
                                    .setMethod(method)
                                    .setArguments(arguments)
                                    .setStream(id));
        } catch (StatusRuntimeException error) {
            return false;
        }
        if (!response.hasError()) return true;
        var code = response.getError().getCode();
        if (code == Error.Code.CODE_STOPPED
                || code == Error.Code.CODE_UNAVAILABLE
                || code == Error.Code.CODE_OVERLOADED
                || method.equals("chunk:report")) return false;
        LOG.log(
                System.Logger.Level.WARNING,
                "Core refused {0} for {1}: {2}",
                method,
                operation,
                response.getError().getMessage());
        results.remove(operation);
        return true;
    }

    private CallResponse call(CallRequest.Builder request) {
        return calls.withDeadlineAfter(5, TimeUnit.SECONDS).call(request.build());
    }

    /** Waits {@code millis} unless the link closes first; waking the link doesn't cut it short. */
    private void pause(long millis) {
        long until = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(millis);
        for (long left = until - System.nanoTime();
                !closed && left > 0;
                left = until - System.nanoTime()) LockSupport.parkNanos(left);
    }

    /** What the link last reported on a stream. */
    private static final class Reported {
        final Map<String, JvmSessionStatus> sessions = new HashMap<>();
        final Map<String, JvmDeliveryStatus> deliveries = new HashMap<>();

        Reported(JvmReport inventory) {
            inventory.getSessionsList().forEach(session -> sessions.put(session.getId(), session));
            inventory
                    .getDeliveriesList()
                    .forEach(delivery -> deliveries.put(delivery.getOperationId(), delivery));
        }
    }

    /** One stream of the topic, whose latest complete view waits for the link's thread. */
    private static final class Stream implements StreamObserver<Update> {
        final AtomicReference<Map<String, ByteString>> latest = new AtomicReference<>();
        final Thread owner;
        volatile @Nullable String id;
        volatile boolean ended;
        private final List<Update> parts = new ArrayList<>();
        private Map<String, ByteString> view = Map.of();

        Stream(Thread owner) {
            this.owner = owner;
        }

        @Override
        public void onNext(Update update) {
            if (update.hasError()) {
                ended = true;
                LockSupport.unpark(owner);
                return;
            }
            parts.add(update);
            if (update.getContinued()) return;
            var next =
                    parts.getFirst().getSnapshot()
                            ? new HashMap<String, ByteString>()
                            : new HashMap<>(view);
            for (var part : parts) {
                for (var entry : part.getUpsertsList()) {
                    if (entry.hasValue()) next.put(entry.getKey(), entry.getValue());
                    else next.remove(entry.getKey());
                }
                part.getRemovedList().forEach(next::remove);
            }
            var named = parts.getFirst().getStream();
            parts.clear();
            view = Map.copyOf(next);
            latest.set(view);
            if (!named.isEmpty()) id = named;
            LockSupport.unpark(owner);
        }

        @Override
        public void onError(Throwable error) {
            ended = true;
            LockSupport.unpark(owner);
        }

        @Override
        public void onCompleted() {
            ended = true;
            LockSupport.unpark(owner);
        }
    }

    /** Core refused the JVM's registration, and will again. */
    private static final class Refused extends RuntimeException {
        private static final long serialVersionUID = 1;

        Refused(Error error) {
            super(error.getCode() + ": " + error.getMessage());
        }
    }
}
