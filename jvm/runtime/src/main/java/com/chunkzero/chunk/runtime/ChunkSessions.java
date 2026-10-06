package com.chunkzero.chunk.runtime;

import chunk.sync.v1.CoreOuterClass.Position;
import chunk.sync.v1.Jvm.JvmDelivery;
import chunk.sync.v1.Jvm.JvmDeliveryPhase;
import chunk.sync.v1.Jvm.JvmDeliveryStatus;
import chunk.sync.v1.Jvm.JvmMethodCall;
import chunk.sync.v1.Jvm.JvmMethodPhase;
import chunk.sync.v1.Jvm.JvmMethodResult;
import chunk.sync.v1.Jvm.JvmReport;
import chunk.sync.v1.Jvm.JvmSession;
import chunk.sync.v1.Jvm.JvmSessionPhase;
import chunk.sync.v1.Jvm.JvmSessionStatus;
import chunk.sync.v1.Jvm.PlayerSetup;

import com.chunkzero.chunk.backend.api.BackendJson;
import com.chunkzero.chunk.backend.api.Destination;
import com.chunkzero.chunk.backend.client.BackendSession;
import com.chunkzero.chunk.runtime.control.ProcessState;
import com.google.protobuf.ByteString;
import com.google.protobuf.InvalidProtocolBufferException;
import com.google.protobuf.Parser;

import org.jetbrains.annotations.Nullable;

import tools.jackson.databind.JsonNode;

import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.time.Duration;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.Set;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;
import java.util.concurrent.Executor;
import java.util.concurrent.Executors;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.function.Function;
import java.util.regex.Pattern;

/**
 * Runs the sessions, player deliveries and session methods core assigns this JVM through a {@link
 * SessionHandler}, and reports them to core. It keeps the accounting core relies on: session
 * phases, capacity, delivery capabilities and fencing, and which methods may run. What a session
 * is, and how it is isolated from the others, is up to the handler.
 *
 * <p>Get one from {@link ChunkProcess#host}. Its methods are thread-safe.
 */
public final class ChunkSessions implements AutoCloseable {
    /** Matches how long control waits for a created session to become ready. */
    static final Duration CREATE_DEADLINE = Duration.ofSeconds(10);

    private static final Pattern SESSION_ID = Pattern.compile("[A-Za-z0-9_-]{1,128}");
    private static final Pattern USERNAME = Pattern.compile("[A-Za-z0-9_]{1,16}");
    private static final int MAX_SESSIONS = 256;
    private static final int MAX_DELIVERIES = 4096;
    private static final int MAX_SETUP = 4096;
    private static final int MAX_JSON = 48 * 1024;

    /** How long a delivery waits for its session to become ready. */
    private static final long WAIT_NANOS = TimeUnit.SECONDS.toNanos(30);

    /**
     * How long the gateway may take to connect the player: it loads their resource packs first.
     * Control cancels the claim of a delivery not activated within 60 seconds of its preparation
     * anyway.
     */
    private static final long CONNECT_NANOS = TimeUnit.SECONDS.toNanos(60);

    private final SessionHandler handler;
    private final Link link;
    private final Executor callbacks;
    private final Duration createDeadline;
    private final @Nullable ScheduledExecutorService worker;
    private final DeliveryFence fence = new DeliveryFence();

    // Everything below is guarded by this.
    private final Map<String, SessionControl> sessions = new HashMap<>();
    // Sessions that failed before they were created, or ended without ever being created.
    private final Map<String, JvmSessionStatus> outcomes = new HashMap<>();
    // The sessions the latest snapshot lists, those it asked to create, and to finish.
    private Set<String> desired = Set.of();
    private final Set<String> created = new HashSet<>();
    private final Set<String> finished = new HashSet<>();
    // The deliveries prepared, the topic's latest, those waiting for their session since when, and
    // those closed without preparing.
    private final Map<String, Delivery> deliveries = new LinkedHashMap<>();
    private Map<String, JvmDelivery> wanted = Map.of();
    private final Map<String, Long> pending = new HashMap<>();
    private final Map<String, JvmDelivery> refused = new HashMap<>();
    // Each method the topic lists, until its key is gone.
    private final Map<String, MethodCall> methods = new HashMap<>();
    private boolean closed;

    /**
     * Runs handler callbacks on {@code callbacks}, and fails sessions not ready within {@code
     * createDeadline}. The owner calls {@link #sweep()} periodically.
     */
    ChunkSessions(
            SessionHandler handler,
            Link link,
            Executor callbacks,
            Duration createDeadline,
            @Nullable ScheduledExecutorService worker) {
        this.handler = Objects.requireNonNull(handler);
        this.link = link;
        this.callbacks = callbacks;
        this.createDeadline = createDeadline;
        this.worker = worker;
    }

    /** Runs callbacks and sweeps on a thread of its own. */
    static ChunkSessions linked(SessionHandler handler, Link link) {
        var worker =
                Executors.newSingleThreadScheduledExecutor(
                        task -> {
                            var thread = new Thread(task, "chunk-sessions");
                            thread.setDaemon(true);
                            return thread;
                        });
        var sessions = new ChunkSessions(handler, link, worker, CREATE_DEADLINE, worker);
        worker.scheduleWithFixedDelay(sessions::sweep, 50, 50, TimeUnit.MILLISECONDS);
        return sessions;
    }

    /**
     * Sessions without core, for tests: callbacks run on the calling thread, moves fail and method
     * results go nowhere. Call {@link #sweep()} to expire deliveries.
     */
    static ChunkSessions detached(
            SessionHandler handler, Function<String, @Nullable BackendSession> backends) {
        return new ChunkSessions(
                handler,
                new Link() {
                    @Override
                    public boolean acceptsWork() {
                        return true;
                    }

                    @Override
                    public @Nullable BackendSession backend(String session) {
                        return backends.apply(session);
                    }

                    @Override
                    public CompletionStage<MoveResult> move(
                            String delivery, Position generation, Destination destination) {
                        return CompletableFuture.failedFuture(
                                new IllegalStateException("Moves unavailable"));
                    }

                    @Override
                    public void methodResult(String operation, JvmMethodResult result) {}

                    @Override
                    public void flush() {}
                },
                Runnable::run,
                CREATE_DEADLINE,
                null);
    }

    /**
     * Admits a player presenting {@code setup}, the payload of their {@code chunk:delivery} login
     * plugin response, as {@code uuid} and {@code name}. Admit the player with the returned
     * delivery's {@link Delivery#player() profile}. {@code disconnect} is called once if the
     * delivery is withdrawn; the engine then disconnects the player and {@linkplain
     * Delivery#release() releases} the delivery.
     *
     * @throws IllegalArgumentException if the setup is invalid or presents another player
     * @throws IllegalStateException if the delivery can no longer be admitted
     */
    public Delivery admit(byte[] setup, UUID uuid, String name, Runnable disconnect) {
        Objects.requireNonNull(uuid);
        Objects.requireNonNull(name);
        Objects.requireNonNull(disconnect);
        if (setup == null || setup.length > MAX_SETUP)
            throw new IllegalArgumentException("Invalid delivery setup");
        PlayerSetup parsed;
        try {
            parsed = PlayerSetup.parseFrom(setup);
        } catch (InvalidProtocolBufferException error) {
            throw new IllegalArgumentException("Invalid delivery setup", error);
        }
        var after = new ArrayList<Runnable>();
        try {
            synchronized (this) {
                var delivery = deliveries.get(parsed.getOperationId());
                if (delivery == null) throw new IllegalArgumentException("Unknown operation");
                expire(delivery, after);
                if (delivery.closed
                        || delivery.consumed
                        || phase(delivery.session()) != JvmSessionPhase.JVM_SESSION_PHASE_READY)
                    throw new IllegalStateException("Delivery unavailable");
                if (!MessageDigest.isEqual(
                        delivery.capability, parsed.getCapability().toByteArray()))
                    throw new IllegalArgumentException("Invalid delivery capability");
                if (!delivery.player().uuid().equals(uuid)
                        || !delivery.player().name().equals(name))
                    throw new IllegalArgumentException("Player identity mismatch");
                fence.claim(uuid.toString(), delivery.generation());
                delivery.consumed = true;
                delivery.disconnect = disconnect;
                return delivery;
            }
        } finally {
            after.forEach(Runnable::run);
            link.flush();
        }
    }

    /** The open delivery {@code id}, or null. */
    public synchronized @Nullable Delivery delivery(String id) {
        var delivery = deliveries.get(id);
        return delivery == null || delivery.closed ? null : delivery;
    }

    /** Sessions starting, ready or ending, which count against this JVM's capacity. */
    public synchronized int activeCount() {
        return (int)
                sessions.values().stream()
                        .filter(
                                session ->
                                        switch (session.phase) {
                                            case JVM_SESSION_PHASE_STARTING,
                                                    JVM_SESSION_PHASE_READY,
                                                    JVM_SESSION_PHASE_ENDING ->
                                                    true;
                                            default -> false;
                                        })
                        .count();
    }

    /** Players admitted and not yet released. */
    public synchronized int players() {
        return (int)
                deliveries.values().stream()
                        .filter(delivery -> delivery.consumed && !delivery.isReleased)
                        .count();
    }

    /** Stops running the topic, and withdraws every delivery. */
    @Override
    public void close() {
        var after = new ArrayList<Runnable>();
        synchronized (this) {
            if (closed) return;
            closed = true;
            deliveries.values().forEach(delivery -> close(delivery, after));
        }
        after.forEach(Runnable::run);
        if (worker != null) worker.shutdown();
    }

    ProcessState state() {
        return new ProcessState() {
            @Override
            public JvmReport inventory() {
                synchronized (ChunkSessions.this) {
                    forget();
                    return ChunkSessions.this.inventory();
                }
            }

            @Override
            public void apply(Map<String, ByteString> entries) {
                ChunkSessions.this.apply(entries);
            }
        };
    }

    synchronized JvmReport inventory() {
        var prepared = new HashMap<String, Integer>();
        var attached = new HashMap<String, Integer>();
        var report = JvmReport.newBuilder();
        for (var delivery : deliveries.values()) {
            var status = delivery.status();
            report.addDeliveries(status);
            if (status.getPhase() == JvmDeliveryPhase.JVM_DELIVERY_PHASE_PREPARED)
                prepared.merge(delivery.session(), 1, Integer::sum);
            if (delivery.consumed && !delivery.isReleased)
                attached.merge(delivery.session(), 1, Integer::sum);
        }
        refused.forEach(
                (operation, delivery) ->
                        report.addDeliveries(
                                JvmDeliveryStatus.newBuilder()
                                        .setOperationId(operation)
                                        .setGeneration(delivery.getGeneration())
                                        .setPhase(JvmDeliveryPhase.JVM_DELIVERY_PHASE_CLOSED)));
        sessions.forEach(
                (id, session) ->
                        report.addSessions(
                                session.status(
                                        prepared.getOrDefault(id, 0),
                                        attached.getOrDefault(id, 0))));
        report.addAllSessions(outcomes.values());
        return report.build();
    }

    /** Drops the records of ended sessions the topic no longer lists. */
    synchronized void forget() {
        sessions.entrySet()
                .removeIf(
                        entry ->
                                !desired.contains(entry.getKey())
                                        && terminal(entry.getValue().phase));
        outcomes.keySet().removeIf(id -> !desired.contains(id));
    }

    /** Makes the JVM's work match the latest snapshot of its topic. */
    void apply(Map<String, ByteString> entries) {
        var listed = entries(entries, "session/", JvmSession.parser());
        var latest = entries(entries, "delivery/", JvmDelivery.parser());
        var calls = entries(entries, "method/", JvmMethodCall.parser());
        var after = new ArrayList<Runnable>();
        synchronized (this) {
            if (closed) return;
            listed.forEach(
                    (id, session) -> {
                        if (created.add(id) && !session.getFinish()) {
                            if (link.acceptsWork()) create(id, session, after);
                            else outcome(id, session, JvmSessionPhase.JVM_SESSION_PHASE_FAILED);
                        }
                        if (session.getFinish() && finished.add(id)) finish(id, session, after);
                    });
            for (var id : Set.copyOf(created)) {
                if (listed.containsKey(id)) continue;
                created.remove(id);
                if (finished.add(id)) finish(id, JvmSession.getDefaultInstance(), after);
            }
            finished.retainAll(created);
            desired = Set.copyOf(listed.keySet());
            applyDeliveries(latest, after);
            applyMethods(calls, after);
        }
        after.forEach(Runnable::run);
    }

    /**
     * Fails sessions not ready within the creation deadline, expires deliveries never connected,
     * refuses those whose session won't come, and reports.
     */
    void sweep() {
        var after = new ArrayList<Runnable>();
        synchronized (this) {
            var now = link.nanoTime();
            for (var control : List.copyOf(sessions.values())) {
                if (!control.settled && now - control.startedAt >= createDeadline.toNanos())
                    fail(
                            control,
                            new TimeoutException(
                                    "Session "
                                            + control.id()
                                            + " was not ready within "
                                            + createDeadline),
                            true,
                            after);
            }
            deliveries.values().forEach(delivery -> expire(delivery, after));
            settle(after);
        }
        after.forEach(Runnable::run);
        link.flush();
    }

    /**
     * Creates session {@code id}, completing once it is ready. A repeat with the same type,
     * capacity and configuration finds the same session.
     */
    CompletableFuture<JvmSessionStatus> create(String id, JvmSession session) {
        var after = new ArrayList<Runnable>();
        CompletableFuture<JvmSessionStatus> result;
        synchronized (this) {
            result = create(id, session, after);
        }
        after.forEach(Runnable::run);
        return result;
    }

    /** Ends session {@code id}. One never created ends at once, reported with {@code session}. */
    CompletableFuture<JvmSessionStatus> finish(String id, JvmSession session) {
        var after = new ArrayList<Runnable>();
        CompletableFuture<JvmSessionStatus> result;
        synchronized (this) {
            result = finish(id, session, after);
        }
        after.forEach(Runnable::run);
        return result;
    }

    /** The phase of session {@code id}, or null while there is no record of it. */
    synchronized @Nullable JvmSessionPhase phase(String id) {
        var session = sessions.get(id);
        if (session != null) return session.phase;
        var outcome = outcomes.get(id);
        return outcome == null ? null : outcome.getPhase();
    }

    static boolean terminal(@Nullable JvmSessionPhase phase) {
        return phase == JvmSessionPhase.JVM_SESSION_PHASE_ENDED
                || phase == JvmSessionPhase.JVM_SESSION_PHASE_FAILED;
    }

    private CompletableFuture<JvmSessionStatus> create(
            String id, JvmSession session, List<Runnable> after) {
        JsonNode configuration;
        try {
            if (!SESSION_ID.matcher(id).matches()
                    || session.getCapacity() < 1
                    || session.getCapacity() > 128)
                throw new IllegalArgumentException("Invalid session");
            configuration = configuration(session);
        } catch (RuntimeException error) {
            if (!sessions.containsKey(id))
                outcome(id, session, JvmSessionPhase.JVM_SESSION_PHASE_FAILED);
            return CompletableFuture.failedFuture(error);
        }
        var previous = sessions.get(id);
        if (previous != null) {
            if (!previous.type().equals(session.getSessionType())
                    || previous.capacity() != session.getCapacity()
                    || !previous.configuration().equals(configuration))
                return CompletableFuture.failedFuture(
                        new IllegalArgumentException("Session creation changed"));
            return previous.created;
        }
        // Control still counts a session it asked for, so it never sees room for one beyond this.
        if (activeCount() >= MAX_SESSIONS)
            return CompletableFuture.failedFuture(
                    new IllegalStateException("Too many live sessions"));
        var control =
                new SessionControl(
                        this, id, session, configuration, link.backend(id), link.nanoTime());
        outcomes.remove(id);
        sessions.put(id, control);
        after.add(
                () ->
                        dispatch(
                                control,
                                () -> {
                                    synchronized (this) {
                                        if (control.handlerFinishing) return;
                                    }
                                    handler.create(control);
                                }));
        return control.created;
    }

    private static JsonNode configuration(JvmSession session) {
        var bytes = session.getConfigurationJson();
        if (bytes.size() > 64 * 1024 || !bytes.isValidUtf8())
            throw new IllegalArgumentException("Invalid session configuration encoding or size");
        var config = BackendJson.mapper().readTree(bytes.isEmpty() ? "{}" : bytes.toStringUtf8());
        if (config == null || !config.isObject())
            throw new IllegalArgumentException("Session configuration must be an object");
        return config;
    }

    private CompletableFuture<JvmSessionStatus> finish(
            String id, JvmSession session, List<Runnable> after) {
        var control = sessions.get(id);
        if (control == null)
            return CompletableFuture.completedFuture(
                    outcome(id, session, JvmSessionPhase.JVM_SESSION_PHASE_ENDED));
        finish(control, after);
        return control.ended.thenApply(ignored -> control.status(0, 0));
    }

    private JvmSessionStatus outcome(String id, JvmSession session, JvmSessionPhase phase) {
        var status =
                JvmSessionStatus.newBuilder()
                        .setId(id)
                        .setSessionType(session.getSessionType())
                        .setCapacity(session.getCapacity())
                        .setPhase(phase)
                        .build();
        outcomes.put(id, status);
        return status;
    }

    boolean ready(SessionControl control) {
        var after = new ArrayList<Runnable>();
        boolean ready;
        synchronized (this) {
            if (control.settled) return false;
            control.settled = true;
            ready = !control.finishing;
            if (ready) control.phase = JvmSessionPhase.JVM_SESSION_PHASE_READY;
            var status = control.status(0, 0);
            after.add(() -> control.created.complete(status));
            // A finish requested while it was starting waited for creation to settle.
            if (control.finishing) withdraw(control, after);
        }
        after.forEach(Runnable::run);
        link.flush();
        return ready;
    }

    void fail(SessionControl control, Throwable error) {
        var after = new ArrayList<Runnable>();
        synchronized (this) {
            fail(control, error, false, after);
        }
        after.forEach(Runnable::run);
        link.flush();
    }

    private void fail(
            SessionControl control, Throwable error, boolean onlyStarting, List<Runnable> after) {
        Objects.requireNonNull(error);
        if (terminal(control.phase) || (onlyStarting && control.settled)) return;
        if (control.failure == null) control.failure = error;
        if (!control.settled) {
            control.settled = true;
            after.add(() -> control.created.completeExceptionally(error));
            if (control.finishing) withdraw(control, after);
        }
        finish(control, after);
    }

    CompletionStage<Void> finish(SessionControl control) {
        var after = new ArrayList<Runnable>();
        synchronized (this) {
            finish(control, after);
        }
        after.forEach(Runnable::run);
        link.flush();
        return control.ended.minimalCompletionStage();
    }

    /** Ends a session once its creation settled: its deliveries close, then its handler ends it. */
    private void finish(SessionControl control, List<Runnable> after) {
        if (control.finishing) return;
        control.finishing = true;
        control.phase = JvmSessionPhase.JVM_SESSION_PHASE_ENDING;
        if (control.settled) withdraw(control, after);
    }

    private void withdraw(SessionControl control, List<Runnable> after) {
        var releases =
                deliveries.values().stream()
                        .filter(delivery -> delivery.session().equals(control.id()))
                        .map(delivery -> close(delivery, after))
                        .toArray(CompletableFuture<?>[]::new);
        after.add(
                () ->
                        CompletableFuture.allOf(releases)
                                .whenComplete(
                                        (ignored, error) -> {
                                            synchronized (this) {
                                                control.handlerFinishing = true;
                                            }
                                            dispatch(
                                                    () -> {
                                                        try {
                                                            handler.finish(control);
                                                        } catch (RuntimeException failure) {
                                                            ended(control, failure);
                                                        }
                                                    });
                                        }));
    }

    void ended(SessionControl control, @Nullable Throwable error) {
        Throwable failure;
        synchronized (this) {
            if (!control.handlerFinishing)
                throw new IllegalStateException("Session is not finishing");
            if (terminal(control.phase)) return;
            if (control.failure == null) control.failure = error;
            failure = control.failure;
            control.phase =
                    failure == null
                            ? JvmSessionPhase.JVM_SESSION_PHASE_ENDED
                            : JvmSessionPhase.JVM_SESSION_PHASE_FAILED;
        }
        var backend = control.backend();
        if (backend != null) backend.close();
        if (failure == null) control.ended.complete(null);
        else control.ended.completeExceptionally(failure);
        link.flush();
    }

    private void applyDeliveries(Map<String, JvmDelivery> latest, List<Runnable> after) {
        wanted = Map.copyOf(latest);
        deliveries.forEach(
                (operation, delivery) -> {
                    var current = wanted.get(operation);
                    if (current == null || current.getWithdraw()) close(delivery, after);
                });
        refused.keySet().retainAll(wanted.keySet());
        pending.keySet().retainAll(wanted.keySet());
        wanted.forEach(
                (operation, delivery) -> {
                    if (!deliveries.containsKey(operation) && !refused.containsKey(operation))
                        pending.putIfAbsent(operation, link.nanoTime());
                });
        settle(after);
    }

    /** Prepares or refuses each waiting delivery it can, and forgets those released and gone. */
    private void settle(List<Runnable> after) {
        for (var operation : List.copyOf(pending.keySet())) {
            var delivery = wanted.get(operation);
            var phase = phase(delivery.getSession());
            if (phase == JvmSessionPhase.JVM_SESSION_PHASE_READY && !delivery.getWithdraw()) {
                try {
                    prepare(operation, delivery);
                } catch (RuntimeException error) {
                    refused.put(operation, delivery);
                }
            } else if (delivery.getWithdraw()
                    || closed
                    || !link.acceptsWork()
                    || (phase != null && phase != JvmSessionPhase.JVM_SESSION_PHASE_STARTING)
                    || link.nanoTime() - pending.get(operation) >= WAIT_NANOS) {
                refused.put(operation, delivery);
            } else continue;
            pending.remove(operation);
        }
        deliveries
                .entrySet()
                .removeIf(
                        entry ->
                                !wanted.containsKey(entry.getKey()) && entry.getValue().isReleased);
    }

    private void prepare(String operation, JvmDelivery delivery) {
        if (closed || !link.acceptsWork()) throw new IllegalStateException("Not accepting work");
        var identity = delivery.getPlayer();
        if (!USERNAME.matcher(identity.getUsername()).matches()
                || !UUID.fromString(identity.getUuid()).toString().equals(identity.getUuid()))
            throw new IllegalArgumentException("Invalid player identity");
        if (deliveries.size() >= MAX_DELIVERIES)
            throw new IllegalStateException("Process delivery capacity reached");
        var session = sessions.get(delivery.getSession());
        var reserved =
                deliveries.values().stream()
                        .filter(
                                candidate ->
                                        candidate.session().equals(delivery.getSession())
                                                && !candidate.isReleased)
                        .count();
        if (reserved >= session.capacity()) throw new IllegalStateException("Session full");
        deliveries.put(operation, new Delivery(this, operation, delivery, link.nanoTime()));
    }

    private void expire(Delivery delivery, List<Runnable> after) {
        if (!delivery.consumed
                && !delivery.closed
                && link.nanoTime() - delivery.openedAt >= CONNECT_NANOS) close(delivery, after);
    }

    /** Closes a delivery: at once if never admitted, or else once the engine releases it. */
    private CompletableFuture<Void> close(Delivery delivery, List<Runnable> after) {
        if (delivery.closed) return delivery.released;
        delivery.closed = true;
        if (!delivery.consumed) {
            delivery.isReleased = true;
            after.add(() -> delivery.released.complete(null));
        } else {
            var disconnect = Objects.requireNonNull(delivery.disconnect);
            dispatchLater(after, disconnect);
        }
        return delivery.released;
    }

    void arrived(Delivery delivery) {
        synchronized (this) {
            if (!delivery.consumed || delivery.closed || delivery.arrived) return;
            delivery.arrived = true;
        }
        link.flush();
    }

    void release(Delivery delivery) {
        synchronized (this) {
            if (delivery.isReleased) return;
            delivery.closed = true;
            delivery.isReleased = true;
            if (delivery.consumed)
                fence.release(delivery.player().uuid().toString(), delivery.generation());
        }
        delivery.released.complete(null);
        link.flush();
    }

    CompletionStage<MoveResult> move(Delivery delivery, Destination destination) {
        return link.move(delivery.id(), delivery.generation(), destination);
    }

    private void applyMethods(Map<String, JvmMethodCall> calls, List<Runnable> after) {
        methods.keySet().retainAll(calls.keySet());
        calls.forEach(
                (id, call) -> {
                    var operation = methods.get(id);
                    if (operation == null) {
                        operation = new MethodCall(id, call);
                        methods.put(id, operation);
                        var queued = operation;
                        dispatchLater(after, () -> execute(queued));
                    }
                    if (call.getCancel() && !operation.started) cancel(operation);
                });
    }

    private void execute(MethodCall operation) {
        var call = operation.call;
        SessionControl session;
        synchronized (this) {
            if (operation.done || methods.get(operation.id) != operation) return;
            session = sessions.get(call.getSession());
            if (closed
                    || session == null
                    || session.phase != JvmSessionPhase.JVM_SESSION_PHASE_READY
                    || link.currentTimeMillis() >= call.getDeadlineMs()
                    || !arrivedIn(call.getDelivery(), call.getSession())) {
                cancel(operation);
                return;
            }
        }
        CompletionStage<String> result;
        try {
            result =
                    handler.method(
                            session,
                            new SessionMethod(
                                    call.getMethod(),
                                    call.getArgumentsJson().toStringUtf8(),
                                    () -> start(operation)));
        } catch (RuntimeException error) {
            result = CompletableFuture.failedFuture(error);
        }
        result.whenComplete(
                (json, error) -> {
                    var failed =
                            JvmMethodResult.newBuilder()
                                    .setPhase(JvmMethodPhase.JVM_METHOD_PHASE_FAILED)
                                    .build();
                    var outcome = failed;
                    if (error == null && json != null) {
                        var encoded = json.getBytes(StandardCharsets.UTF_8);
                        if (encoded.length <= MAX_JSON)
                            outcome =
                                    JvmMethodResult.newBuilder()
                                            .setPhase(JvmMethodPhase.JVM_METHOD_PHASE_COMPLETED)
                                            .setResultJson(ByteString.copyFrom(encoded))
                                            .build();
                    }
                    synchronized (this) {
                        if (operation.started) complete(operation, outcome);
                        else cancel(operation);
                    }
                });
    }

    /** Marks {@code operation} started if it may still run, and cancels it otherwise. */
    private synchronized boolean start(MethodCall operation) {
        if (operation.done || methods.get(operation.id) != operation) return false;
        if (operation.started) return true;
        var call = operation.call;
        var session = sessions.get(call.getSession());
        if (closed
                || session == null
                || session.phase != JvmSessionPhase.JVM_SESSION_PHASE_READY
                || link.currentTimeMillis() >= call.getDeadlineMs()
                || !arrivedIn(call.getDelivery(), call.getSession())) {
            cancel(operation);
            return false;
        }
        operation.started = true;
        return true;
    }

    private boolean arrivedIn(String operation, String session) {
        var delivery = deliveries.get(operation);
        return delivery != null
                && delivery.arrived
                && !delivery.closed
                && delivery.session().equals(session);
    }

    private void cancel(MethodCall operation) {
        complete(
                operation,
                JvmMethodResult.newBuilder()
                        .setPhase(JvmMethodPhase.JVM_METHOD_PHASE_CANCELLED)
                        .build());
    }

    private void complete(MethodCall operation, JvmMethodResult result) {
        if (operation.done) return;
        operation.done = true;
        if (methods.get(operation.id) == operation) link.methodResult(operation.id, result);
    }

    /** Runs a handler callback for {@code control}, failing the session if it throws. */
    private void dispatch(SessionControl control, Runnable callback) {
        dispatch(
                () -> {
                    try {
                        callback.run();
                    } catch (RuntimeException error) {
                        fail(control, error);
                    }
                });
    }

    private void dispatchLater(List<Runnable> after, Runnable callback) {
        after.add(() -> dispatch(callback));
    }

    private void dispatch(Runnable callback) {
        try {
            callbacks.execute(callback);
        } catch (RejectedExecutionException ignored) {
            // Closed: the process is stopping.
        }
    }

    /** The entries under {@code prefix}, by the rest of their key, skipping undecodable ones. */
    private static <T> Map<String, T> entries(
            Map<String, ByteString> entries, String prefix, Parser<T> parser) {
        var parsed = new HashMap<String, T>();
        entries.forEach(
                (key, value) -> {
                    if (!key.startsWith(prefix)) return;
                    try {
                        parsed.put(key.substring(prefix.length()), parser.parseFrom(value));
                    } catch (InvalidProtocolBufferException ignored) {
                        // Not an entry this runtime can run.
                    }
                });
        return parsed;
    }

    private static final class MethodCall {
        final String id;
        final JvmMethodCall call;
        boolean started;
        boolean done;

        MethodCall(String id, JvmMethodCall call) {
            this.id = id;
            this.call = call;
        }
    }

    /** What sessions need from the process and core. */
    interface Link {
        /** Whether the process accepts new sessions and players. */
        boolean acceptsWork();

        @Nullable
        BackendSession backend(String session);

        CompletionStage<MoveResult> move(
                String delivery, Position generation, Destination destination);

        void methodResult(String operation, JvmMethodResult result);

        /** Reports what changed to core. */
        void flush();

        default long nanoTime() {
            return System.nanoTime();
        }

        default long currentTimeMillis() {
            return System.currentTimeMillis();
        }
    }
}
