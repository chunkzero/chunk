package dev.chunkzero.runtime.minestom.internal;

import chunk.v1.Common.Error;
import chunk.v1.SessionMethodsGrpc;
import chunk.v1.SessionMethodsOuterClass.SessionMethodPhase;
import chunk.v1.SessionMethodsOuterClass.SessionMethodRequest;
import chunk.v1.SessionMethodsOuterClass.SessionMethodResult;
import chunk.v1.Supervision.ProcessIdentity;

import dev.chunkzero.runtime.SessionManager;
import dev.chunkzero.runtime.SessionMethodBinding;

import io.grpc.Status;
import io.grpc.stub.StreamObserver;

import org.jetbrains.annotations.ApiStatus;

import java.nio.charset.StandardCharsets;
import java.util.Map;
import java.util.TreeMap;
import java.util.function.Consumer;
import java.util.function.LongSupplier;

/** Authenticated process RPC; method admission and execution are separate bounded steps. */
@ApiStatus.Internal
public final class SessionMethodService extends SessionMethodsGrpc.SessionMethodsImplBase {
    private static final int MAX_JSON = 48 * 1024;
    private static final int MAX_PENDING = 128;
    private static final int MAX_RECORDS = 4096;
    private static final long MAX_BYTES = 16 * 1024 * 1024;
    private static final long RETENTION_MS = 300_000;
    private final ProcessIdentity identity;
    private final SessionManager manager;
    private final Map<String, SessionMethodBinding<?, ?>> bindings;
    private final Consumer<SessionMethodRequest> authorizeCaller;
    private final LongSupplier now;
    private final TreeMap<Long, Operation> operations = new TreeMap<>();
    private long retirementFloor;
    private long retainedBytes;
    private int pending;
    private int scheduled;
    private boolean closed;

    public SessionMethodService(
            ProcessIdentity identity,
            SessionManager manager,
            Map<String, SessionMethodBinding<?, ?>> bindings,
            Consumer<SessionMethodRequest> authorizeCaller,
            LongSupplier now) {
        this.identity = identity;
        this.manager = manager;
        this.bindings = Map.copyOf(bindings);
        this.authorizeCaller = authorizeCaller;
        this.now = now;
    }

    @Override
    public void call(SessionMethodRequest request, StreamObserver<SessionMethodResult> response) {
        respond(request, response, false);
    }

    @Override
    public void cancel(SessionMethodRequest request, StreamObserver<SessionMethodResult> response) {
        respond(request, response, true);
    }

    private void respond(
            SessionMethodRequest request,
            StreamObserver<SessionMethodResult> response,
            boolean cancel) {
        try {
            var result = accept(request, cancel);
            response.onNext(result);
            response.onCompleted();
        } catch (io.grpc.StatusRuntimeException error) {
            response.onError(error);
        } catch (RuntimeException error) {
            response.onError(
                    Status.FAILED_PRECONDITION
                            .withDescription("Session method rejected")
                            .asRuntimeException());
        }
    }

    private synchronized SessionMethodResult accept(SessionMethodRequest request, boolean cancel) {
        validateIdentity(request);
        expire();
        var operation = operations.get(request.getSequence());
        if (operation != null) {
            if (!operation.request.equals(request))
                throw Status.ALREADY_EXISTS
                        .withDescription("Operation changed")
                        .asRuntimeException();
            if (cancel) cancel(operation);
            return snapshot(operation);
        }
        if (request.getSequence() <= retirementFloor
                || request.getDeadlineMs() <= now.getAsLong()) {
            retirementFloor = Math.max(retirementFloor, request.getSequence());
            return result(request, SessionMethodPhase.SESSION_METHOD_PHASE_UNKNOWN);
        }
        var binding = bindings.get(request.getSessionType() + "/" + request.getMethod());
        if (binding == null || !binding.reference().app().equals(identity.getAppId())) {
            throw Status.NOT_FOUND
                    .withDescription("Undeclared session method")
                    .asRuntimeException();
        }
        binding.reference().arguments().read(request.getArgumentsJson());
        if (!cancel) {
            if (closed || pending >= MAX_PENDING || scheduled >= MAX_PENDING)
                throw Status.RESOURCE_EXHAUSTED.asRuntimeException();
            validateLive(request);
        }
        if (!makeRoom(request.getSerializedSize()))
            throw Status.RESOURCE_EXHAUSTED.asRuntimeException();
        operation = new Operation(request);
        operations.put(request.getSequence(), operation);
        retainedBytes += operation.bytes;
        pending++;
        if (cancel) cancel(operation);
        else {
            var accepted = operation;
            accepted.queued = true;
            scheduled++;
            manager.getTicks()
                    .submit(
                            () -> {
                                try {
                                    execute(accepted, binding);
                                } finally {
                                    synchronized (this) {
                                        accepted.queued = false;
                                        scheduled--;
                                    }
                                }
                                return null;
                            });
        }
        return snapshot(operation);
    }

    private void execute(Operation operation, SessionMethodBinding<?, ?> binding) {
        synchronized (this) {
            if (operation.phase != SessionMethodPhase.SESSION_METHOD_PHASE_ACCEPTED) return;
            try {
                if (closed || now.getAsLong() >= operation.request.getDeadlineMs())
                    throw new IllegalStateException("Deadline");
                validateLive(operation.request);
            } catch (RuntimeException error) {
                cancel(operation);
                return;
            }
            operation.running = true;
        }
        SessionMethodResult completed;
        try {
            var request = operation.request;
            var session = manager.get(request.getSession().getId(), request.getSessionGeneration());
            var json = session.invokeMethod(binding, request.getArgumentsJson());
            if (json.getBytes(StandardCharsets.UTF_8).length > MAX_JSON)
                throw new IllegalArgumentException("Result size limit");
            completed =
                    result(request, SessionMethodPhase.SESSION_METHOD_PHASE_COMPLETED).toBuilder()
                            .setResultJson(json)
                            .build();
        } catch (RuntimeException error) {
            completed =
                    result(operation.request, SessionMethodPhase.SESSION_METHOD_PHASE_FAILED)
                            .toBuilder()
                            .setError(
                                    Error.newBuilder()
                                            .setCode("SESSION_METHOD_FAILED")
                                            .setMessage("Gameplay method failed"))
                            .build();
        }
        synchronized (this) {
            operation.running = false;
            operation.phase = completed.getPhase();
            operation.result = completed;
            operation.finishedAt = now.getAsLong();
            pending--;
            retainedBytes += completed.getSerializedSize();
            operation.bytes += completed.getSerializedSize();
            makeRoom(0);
        }
    }

    private void validateIdentity(SessionMethodRequest request) {
        var clock = now.getAsLong();
        if (!identity.equals(request.getIdentity()))
            throw Status.PERMISSION_DENIED
                    .withDescription("Process identity mismatch")
                    .asRuntimeException();
        if (request.getSequence() <= 0
                || !request.getOperationId()
                        .equals(identity.getProcessId() + "/" + request.getSequence())
                || request.getSerializedSize() > MAX_JSON + 8192
                || request.getArgumentsJson().getBytes(StandardCharsets.UTF_8).length > MAX_JSON
                || request.getSessionGeneration() <= 0
                || request.getSession().getId().isEmpty()
                || request.getIssuedAtMs() <= 0
                || request.getIssuedAtMs() > clock + 1000
                || request.getDeadlineMs() <= request.getIssuedAtMs()
                || request.getDeadlineMs() - request.getIssuedAtMs() > 30_000
                || request.getCaller().getMembershipGeneration() <= 0
                || request.getCaller().getOwnerGeneration() <= 0
                || request.getCaller().getDeliveryOperationId().isEmpty()
                || request.getCaller().getPlayer().getId().isEmpty()) {
            throw Status.INVALID_ARGUMENT
                    .withDescription("Invalid method operation")
                    .asRuntimeException();
        }
    }

    private void validateLive(SessionMethodRequest request) {
        var session = manager.get(request.getSession().getId(), request.getSessionGeneration());
        session.requireMethodReady();
        if (!session.getCommand().getSessionType().equals(request.getSessionType()))
            throw new IllegalArgumentException("Session type mismatch");
        authorizeCaller.accept(request);
    }

    /**
     * Cancels queued work on departed players or disposed sessions and retires expired result
     * records.
     */
    public synchronized void flush() {
        for (var operation : operations.values()) {
            if (operation.phase == SessionMethodPhase.SESSION_METHOD_PHASE_ACCEPTED
                    && !operation.running) {
                try {
                    validateLive(operation.request);
                } catch (RuntimeException error) {
                    cancel(operation);
                }
            }
        }
        expire();
    }

    public synchronized void close() {
        closed = true;
        operations.values().forEach(this::cancel);
    }

    private void cancel(Operation operation) {
        if (operation.phase != SessionMethodPhase.SESSION_METHOD_PHASE_ACCEPTED) return;
        if (operation.running) {
            operation.uncertain = true;
            return;
        }
        operation.phase = SessionMethodPhase.SESSION_METHOD_PHASE_CANCELLED;
        operation.finishedAt = now.getAsLong();
        pending--;
    }

    private void expire() {
        var clock = now.getAsLong();
        for (var operation : operations.values()) {
            if (clock >= operation.request.getDeadlineMs()) cancel(operation);
        }
        var iterator = operations.entrySet().iterator();
        while (iterator.hasNext()) {
            var operation = iterator.next().getValue();
            if (!operation.queued
                    && operation.phase != SessionMethodPhase.SESSION_METHOD_PHASE_ACCEPTED
                    && clock - operation.finishedAt >= RETENTION_MS) {
                retire(operation);
                iterator.remove();
            }
        }
    }

    private boolean makeRoom(long additional) {
        while (operations.size() >= MAX_RECORDS || retainedBytes + additional > MAX_BYTES) {
            var retired =
                    operations.values().stream()
                            .filter(
                                    operation ->
                                            !operation.queued
                                                    && operation.phase
                                                            != SessionMethodPhase
                                                                    .SESSION_METHOD_PHASE_ACCEPTED)
                            .findFirst()
                            .orElse(null);
            if (retired == null) return false;
            retire(retired);
            operations.remove(retired.request.getSequence());
        }
        return true;
    }

    private void retire(Operation operation) {
        retirementFloor = Math.max(retirementFloor, operation.request.getSequence());
        retainedBytes -= operation.bytes;
    }

    private static SessionMethodResult snapshot(Operation operation) {
        if (operation.result != null) return operation.result;
        return result(
                operation.request,
                operation.uncertain
                        ? SessionMethodPhase.SESSION_METHOD_PHASE_UNKNOWN
                        : operation.phase);
    }

    private static SessionMethodResult result(
            SessionMethodRequest request, SessionMethodPhase phase) {
        return SessionMethodResult.newBuilder()
                .setOperationId(request.getOperationId())
                .setPhase(phase)
                .build();
    }

    private static final class Operation {
        final SessionMethodRequest request;
        SessionMethodPhase phase = SessionMethodPhase.SESSION_METHOD_PHASE_ACCEPTED;
        boolean running;
        boolean queued;
        boolean uncertain;
        long finishedAt;
        long bytes;
        SessionMethodResult result;

        Operation(SessionMethodRequest request) {
            this.request = request;
            bytes = request.getSerializedSize();
        }
    }
}
