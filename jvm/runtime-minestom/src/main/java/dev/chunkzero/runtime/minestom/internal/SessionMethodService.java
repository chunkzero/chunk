package dev.chunkzero.runtime.minestom.internal;

import chunk.sync.v1.Jvm.JvmMethodCall;
import chunk.sync.v1.Jvm.JvmMethodPhase;
import chunk.sync.v1.Jvm.JvmMethodResult;

import com.google.protobuf.ByteString;

import dev.chunkzero.runtime.SessionManager;
import dev.chunkzero.runtime.SessionMethodBinding;

import org.jetbrains.annotations.ApiStatus;

import java.nio.charset.StandardCharsets;
import java.util.HashMap;
import java.util.Map;
import java.util.function.BiConsumer;
import java.util.function.BiPredicate;
import java.util.function.LongSupplier;

/**
 * Runs each session method core's topic lists once, on the tick thread, and hands its result to
 * {@code results}. A method starts only before its deadline, unless cancelled, and while its player
 * is arrived in its session; otherwise it is cancelled.
 */
@ApiStatus.Internal
public final class SessionMethodService {
    private static final int MAX_JSON = 48 * 1024;
    private final SessionManager manager;
    private final Map<String, SessionMethodBinding<?, ?>> bindings;
    private final BiPredicate<String, String> arrived;
    private final BiConsumer<String, JvmMethodResult> results;
    private final LongSupplier now;
    // Each method the topic lists, until its key is gone.
    private final Map<String, Operation> operations = new HashMap<>();
    private boolean closed;

    /**
     * {@code arrived} tells whether a delivery's player is in a session, and {@code now} is Unix
     * time in milliseconds.
     */
    public SessionMethodService(
            SessionManager manager,
            Map<String, SessionMethodBinding<?, ?>> bindings,
            BiPredicate<String, String> arrived,
            BiConsumer<String, JvmMethodResult> results,
            LongSupplier now) {
        this.manager = manager;
        this.bindings = Map.copyOf(bindings);
        this.arrived = arrived;
        this.results = results;
        this.now = now;
    }

    /** Runs the topic's latest methods, keyed by operation ID. */
    public synchronized void apply(Map<String, JvmMethodCall> calls) {
        operations.keySet().retainAll(calls.keySet());
        calls.forEach(
                (id, call) -> {
                    var operation = operations.get(id);
                    if (operation == null) {
                        operation = new Operation(id, call);
                        operations.put(id, operation);
                        var queued = operation;
                        manager.getTicks()
                                .submit(
                                        () -> {
                                            execute(queued);
                                            return null;
                                        });
                    }
                    if (call.getCancel() && !operation.started) cancel(operation);
                });
    }

    public synchronized void close() {
        closed = true;
    }

    private void execute(Operation operation) {
        var call = operation.call;
        SessionMethodBinding<?, ?> binding;
        SessionManager.ManagedSession session;
        synchronized (this) {
            if (operation.done || operations.get(operation.id) != operation) return;
            try {
                if (closed
                        || now.getAsLong() >= call.getDeadlineMs()
                        || !arrived.test(call.getDelivery(), call.getSession()))
                    throw new IllegalStateException("The method may not start");
                session = manager.get(call.getSession());
                session.requireMethodReady();
            } catch (RuntimeException error) {
                cancel(operation);
                return;
            }
            binding = bindings.get(session.getSessionType() + "/" + call.getMethod());
            operation.started = true;
        }
        JvmMethodResult result;
        try {
            if (binding == null) throw new IllegalArgumentException("Undeclared session method");
            var json = session.invokeMethod(binding, call.getArgumentsJson().toStringUtf8());
            var encoded = json.getBytes(StandardCharsets.UTF_8);
            if (encoded.length > MAX_JSON) throw new IllegalArgumentException("Result size limit");
            result =
                    JvmMethodResult.newBuilder()
                            .setPhase(JvmMethodPhase.JVM_METHOD_PHASE_COMPLETED)
                            .setResultJson(ByteString.copyFrom(encoded))
                            .build();
        } catch (RuntimeException error) {
            result =
                    JvmMethodResult.newBuilder()
                            .setPhase(JvmMethodPhase.JVM_METHOD_PHASE_FAILED)
                            .build();
        }
        synchronized (this) {
            finish(operation, result);
        }
    }

    private void cancel(Operation operation) {
        finish(
                operation,
                JvmMethodResult.newBuilder()
                        .setPhase(JvmMethodPhase.JVM_METHOD_PHASE_CANCELLED)
                        .build());
    }

    private void finish(Operation operation, JvmMethodResult result) {
        if (operation.done) return;
        operation.done = true;
        if (operations.get(operation.id) == operation) results.accept(operation.id, result);
    }

    private static final class Operation {
        final String id;
        final JvmMethodCall call;
        boolean started;
        boolean done;

        Operation(String id, JvmMethodCall call) {
            this.id = id;
            this.call = call;
        }
    }
}
