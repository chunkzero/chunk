package dev.chunkzero.runtime;

import chunk.sync.v1.Jvm.JvmSession;
import chunk.sync.v1.Jvm.JvmSessionPhase;
import chunk.sync.v1.Jvm.JvmSessionStatus;

import dev.chunkzero.backend.api.BackendJson;
import dev.chunkzero.backend.client.BackendSession;
import dev.chunkzero.runtime.minestom.event.SessionCreateEvent;
import dev.chunkzero.runtime.minestom.event.SessionJoinEvent;
import dev.chunkzero.runtime.minestom.event.SessionLeaveEvent;
import dev.chunkzero.runtime.minestom.internal.ComponentRegistry;

import net.minestom.server.ServerProcess;
import net.minestom.server.entity.Player;

import org.jetbrains.annotations.ApiStatus;
import org.jetbrains.annotations.Nullable;

import tools.jackson.databind.JsonNode;

import java.util.ArrayList;
import java.util.Collections;
import java.util.IdentityHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;
import java.util.concurrent.CompletionStage;
import java.util.concurrent.ConcurrentHashMap;
import java.util.function.BiFunction;
import java.util.function.Function;
import java.util.function.Supplier;
import java.util.regex.Pattern;
import java.util.stream.Collectors;

@ApiStatus.Internal
public final class SessionManager {
    private static final Pattern SESSION_ID = Pattern.compile("[A-Za-z0-9_-]{1,128}");

    private final ServerProcess process;
    private final TickExecutor ticks;
    private final Map<String, SessionRegistration> factories;
    private final @Nullable BiFunction<String, String, BackendSession> backend;
    private final ComponentRegistry components;
    private final Map<String, ManagedSession> sessions = new ConcurrentHashMap<>();

    /** Sessions that failed to start or were never created, as reported to control. */
    private final Map<String, JvmSessionStatus> outcomes = new ConcurrentHashMap<>();

    private Function<String, CompletionStage<Void>> withdraw =
            ignored -> CompletableFuture.completedFuture(null);
    private SessionScope.Mover mover = SessionScope.Mover.UNAVAILABLE;

    SessionManager(
            ServerProcess process, TickExecutor ticks, Map<String, Supplier<Session>> factories) {
        this(
                process,
                ticks,
                factories.entrySet().stream()
                        .collect(
                                Collectors.toMap(
                                        Map.Entry::getKey,
                                        entry ->
                                                new SessionRegistration(
                                                        entry.getKey(), entry.getValue()))),
                null);
    }

    SessionManager(
            ServerProcess process,
            TickExecutor ticks,
            Map<String, SessionRegistration> factories,
            @Nullable BiFunction<String, String, BackendSession> backend) {
        this(process, ticks, factories, backend, new ComponentRegistry(List.of()));
    }

    SessionManager(
            ServerProcess process,
            TickExecutor ticks,
            Map<String, SessionRegistration> factories,
            @Nullable BiFunction<String, String, BackendSession> backend,
            ComponentRegistry components) {
        this.process = process;
        this.ticks = ticks;
        this.factories = Map.copyOf(factories);
        this.backend = backend;
        this.components = components;
    }

    /**
     * Completes after the tick thread runs the work queued before it, such as session creations.
     */
    public CompletableFuture<Void> afterQueued() {
        return ticks.submit(() -> null);
    }

    public TickExecutor getTicks() {
        return ticks;
    }

    public ServerProcess getProcess() {
        return process;
    }

    public void setWithdraw(Function<String, CompletionStage<Void>> withdraw) {
        this.withdraw = withdraw;
    }

    void setMover(SessionScope.Mover mover) {
        this.mover = mover;
    }

    /**
     * Creates the session, or reports it failed if it cannot start. Beyond 256 live sessions it
     * reports nothing, since control still counts the session and so never sees room for it.
     */
    public CompletableFuture<JvmSessionStatus> create(String id, JvmSession session) {
        return ticks.submit(
                        () -> {
                            if (!sessions.containsKey(id) && activeCount() >= 256)
                                throw new IllegalStateException("Too many live sessions");
                            try {
                                return start(id, session);
                            } catch (RuntimeException error) {
                                if (!sessions.containsKey(id))
                                    outcome(id, session, JvmSessionPhase.JVM_SESSION_PHASE_FAILED);
                                throw error;
                            }
                        })
                .thenCompose(managed -> managed.ready.thenApply(ignored -> managed.status()));
    }

    /** Reports a session this process will not create, such as one requested while draining. */
    public void reject(String id, JvmSession session) {
        ticks.submit(
                () -> {
                    if (!sessions.containsKey(id))
                        outcome(id, session, JvmSessionPhase.JVM_SESSION_PHASE_FAILED);
                    return null;
                });
    }

    private ManagedSession start(String id, JvmSession session) {
        if (!SESSION_ID.matcher(id).matches()
                || session.getCapacity() < 1
                || session.getCapacity() > 128) {
            throw new IllegalArgumentException("Invalid session");
        }
        var configuration = configuration(session);
        var previous = sessions.get(id);
        if (previous != null) {
            if (!previous.session.getSessionType().equals(session.getSessionType())
                    || previous.session.getCapacity() != session.getCapacity()
                    || !configuration(previous.session).equals(configuration))
                throw new IllegalArgumentException("Session creation changed");
            return previous;
        }
        var factory = factories.get(session.getSessionType());
        if (factory == null) throw new IllegalArgumentException("Unknown session type");
        var managed = new ManagedSession(id, session, factory, configuration.toString());
        outcomes.remove(id);
        sessions.put(id, managed);
        managed.start();
        return managed;
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

    /**
     * Ends session {@code id}. One that never started ends at once, reported with {@code session}'s
     * type and capacity.
     */
    public CompletableFuture<JvmSessionStatus> finish(String id, JvmSession session) {
        var current = sessions.get(id);
        if (current != null) current.methodsClosed = true;
        return ticks.submit(
                        () -> {
                            var managed = sessions.get(id);
                            if (managed == null)
                                return CompletableFuture.completedFuture(
                                        outcome(
                                                id,
                                                session,
                                                JvmSessionPhase.JVM_SESSION_PHASE_ENDED));
                            return managed.finish().thenApply(ignored -> managed.status());
                        })
                .thenCompose(Function.identity());
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

    /** Drops the record of a session control no longer tracks, once it can hold nothing. */
    public void forget(String id) {
        ticks.submit(
                () -> {
                    var session = sessions.get(id);
                    if (session == null || terminal(session.phase)) {
                        sessions.remove(id);
                        outcomes.remove(id);
                    }
                    return null;
                });
    }

    public static boolean terminal(JvmSessionPhase phase) {
        return phase == JvmSessionPhase.JVM_SESSION_PHASE_ENDED
                || phase == JvmSessionPhase.JVM_SESSION_PHASE_FAILED;
    }

    /** The phase of session {@code id}, or null while this manager has no record of it. */
    public @Nullable JvmSessionPhase phase(String id) {
        var session = sessions.get(id);
        if (session != null) return session.phase;
        var outcome = outcomes.get(id);
        return outcome == null ? null : outcome.getPhase();
    }

    public ManagedSession get(String id) {
        var session = sessions.get(id);
        if (session == null) throw new IllegalArgumentException("Unknown session");
        if (session.phase != JvmSessionPhase.JVM_SESSION_PHASE_READY)
            throw new IllegalStateException("Session unavailable");
        return session;
    }

    public List<JvmSessionStatus> inventory() {
        var inventory = new ArrayList<JvmSessionStatus>();
        sessions.values().forEach(session -> inventory.add(session.status()));
        inventory.addAll(outcomes.values());
        return inventory;
    }

    int activeCount() {
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

    @ApiStatus.Internal
    public final class ManagedSession {
        private final String id;
        private final JvmSession session;
        private final Session behavior;
        private final SessionScope scope;
        private final CompletableFuture<Void> ready = new CompletableFuture<>();
        private final CompletableFuture<Void> ended = new CompletableFuture<>();
        private final Set<Player> joined = Collections.newSetFromMap(new IdentityHashMap<>());
        private volatile JvmSessionPhase phase = JvmSessionPhase.JVM_SESSION_PHASE_STARTING;
        private boolean finishing;
        private volatile boolean methodsClosed;
        private @Nullable Throwable creationFailure;

        ManagedSession(
                String id,
                JvmSession session,
                SessionRegistration registration,
                String configuration) {
            this.id = id;
            this.session = session;
            behavior = registration.create(session.getCapacity(), configuration);
            scope =
                    new SessionScope(
                            process,
                            id,
                            ticks,
                            this::finish,
                            registration.backend(id, backend),
                            components,
                            (delivery, generation, destination) ->
                                    mover.move(delivery, generation, destination));
        }

        public String getSessionType() {
            return session.getSessionType();
        }

        public int getCapacity() {
            return session.getCapacity();
        }

        public JvmSessionPhase getPhase() {
            return phase;
        }

        public SessionScope getScope() {
            return scope;
        }

        public String invokeMethod(SessionMethodBinding<?, ?> binding, String arguments) {
            ticks.checkThread();
            requireMethodReady();
            return binding.invoke(behavior, arguments);
        }

        public void requireMethodReady() {
            if (methodsClosed || phase != JvmSessionPhase.JVM_SESSION_PHASE_READY)
                throw new IllegalStateException("Session unavailable");
        }

        void start() {
            invoke(() -> behavior.onCreate(scope))
                    .whenComplete(
                            (ignored, error) ->
                                    ticks.submit(
                                            () -> {
                                                if (error == null
                                                        && !scope.getInstances().isEmpty()) {
                                                    if (!finishing)
                                                        phase =
                                                                JvmSessionPhase
                                                                        .JVM_SESSION_PHASE_READY;
                                                    process.eventHandler()
                                                            .call(new SessionCreateEvent(scope));
                                                    ready.complete(null);
                                                } else {
                                                    creationFailure =
                                                            error == null
                                                                    ? new IllegalStateException(
                                                                            "Session has no"
                                                                                    + " instances")
                                                                    : error;
                                                    ready.completeExceptionally(creationFailure);
                                                    finish();
                                                }
                                                return null;
                                            }));
        }

        public CompletableFuture<Void> join(Player player) {
            return ticks.submit(
                            () -> {
                                if (phase != JvmSessionPhase.JVM_SESSION_PHASE_READY)
                                    throw new IllegalStateException("Session unavailable");
                                scope.getPlayers().add(player);
                                return invoke(() -> behavior.onJoin(player));
                            })
                    .thenCompose(Function.identity())
                    .thenCompose(
                            ignored ->
                                    ticks.submit(
                                            () -> {
                                                if (scope.getPlayers().contains(player)
                                                        && joined.add(player)) {
                                                    process.eventHandler()
                                                            .call(
                                                                    new SessionJoinEvent(
                                                                            scope, player));
                                                }
                                                return null;
                                            }));
        }

        public CompletableFuture<Void> leave(Player player) {
            return ticks.submit(
                            () -> {
                                if (!scope.getPlayers().remove(player))
                                    return CompletableFuture.<Void>completedFuture(null);
                                Exception disposalFailure = null;
                                try {
                                    scope.releasePlayer(player);
                                } catch (Exception error) {
                                    disposalFailure = error;
                                }
                                var failure = disposalFailure;
                                return invoke(() -> behavior.onLeave(player))
                                        .handle(
                                                (ignored, error) -> {
                                                    if (failure != null && error != null)
                                                        failure.addSuppressed(error);
                                                    return notifyLeave(
                                                            player,
                                                            failure == null ? error : failure);
                                                })
                                        .thenCompose(Function.identity());
                            })
                    .thenCompose(Function.identity());
        }

        private CompletableFuture<Void> notifyLeave(Player player, @Nullable Throwable failure) {
            return ticks.submit(
                    () -> {
                        if (joined.remove(player))
                            process.eventHandler().call(new SessionLeaveEvent(scope, player));
                        if (failure != null) throw new CompletionException(failure);
                        return null;
                    });
        }

        CompletableFuture<Void> finish() {
            methodsClosed = true;
            ticks.submit(
                    () -> {
                        if (!finishing) {
                            finishing = true;
                            phase = JvmSessionPhase.JVM_SESSION_PHASE_ENDING;
                            // Creation must settle before scoped resources can be disposed.
                            ready.handle((ignored, error) -> null)
                                    .thenCompose(ignored -> withdraw.apply(id))
                                    .thenCompose(
                                            ignored ->
                                                    ticks.submit(() -> invoke(behavior::onFinish))
                                                            .thenCompose(Function.identity()))
                                    .whenComplete(
                                            (ignored, error) ->
                                                    ticks.submit(
                                                            () -> {
                                                                try {
                                                                    scope.dispose();
                                                                } catch (Exception failure) {
                                                                    // An unclosed resource keeps
                                                                    // the
                                                                    // session ending and counted.
                                                                    ended.completeExceptionally(
                                                                            failure);
                                                                    return null;
                                                                }
                                                                var failure =
                                                                        error == null
                                                                                ? creationFailure
                                                                                : error;
                                                                phase =
                                                                        failure == null
                                                                                ? JvmSessionPhase
                                                                                        .JVM_SESSION_PHASE_ENDED
                                                                                : JvmSessionPhase
                                                                                        .JVM_SESSION_PHASE_FAILED;
                                                                if (failure == null)
                                                                    ended.complete(null);
                                                                else
                                                                    ended.completeExceptionally(
                                                                            failure);
                                                                return null;
                                                            }));
                        }
                        return null;
                    });
            return ended;
        }

        JvmSessionStatus status() {
            return JvmSessionStatus.newBuilder()
                    .setId(id)
                    .setSessionType(session.getSessionType())
                    .setCapacity(session.getCapacity())
                    .setPhase(phase)
                    .setAttached(scope.getPlayers().size())
                    .build();
        }
    }

    private static CompletableFuture<Void> invoke(Supplier<CompletionStage<Void>> action) {
        try {
            return action.get().toCompletableFuture();
        } catch (Exception error) {
            return CompletableFuture.failedFuture(error);
        }
    }
}
