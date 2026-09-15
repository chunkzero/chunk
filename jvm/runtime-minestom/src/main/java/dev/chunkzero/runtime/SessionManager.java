package dev.chunkzero.runtime;

import chunk.v1.Supervision.SessionCommand;
import chunk.v1.Supervision.SessionInventory;
import chunk.v1.Supervision.SessionPhase;

import dev.chunkzero.backend.client.BackendSession;
import dev.chunkzero.runtime.minestom.event.SessionCreateEvent;
import dev.chunkzero.runtime.minestom.event.SessionJoinEvent;
import dev.chunkzero.runtime.minestom.event.SessionLeaveEvent;
import dev.chunkzero.runtime.minestom.internal.ComponentRegistry;

import net.minestom.server.entity.Player;
import net.minestom.server.event.EventDispatcher;

import org.jetbrains.annotations.ApiStatus;
import org.jetbrains.annotations.Nullable;

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

    private final TickExecutor ticks;
    private final Map<String, SessionRegistration> factories;
    private final @Nullable BiFunction<String, String, BackendSession> backend;
    private final ComponentRegistry components;
    private final Map<String, ManagedSession> sessions = new ConcurrentHashMap<>();
    private Function<String, CompletionStage<Void>> withdraw =
            ignored -> CompletableFuture.completedFuture(null);

    SessionManager(TickExecutor ticks, Map<String, Supplier<Session>> factories) {
        this(
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
            TickExecutor ticks,
            Map<String, SessionRegistration> factories,
            @Nullable BiFunction<String, String, BackendSession> backend) {
        this(ticks, factories, backend, new ComponentRegistry(List.of()));
    }

    SessionManager(
            TickExecutor ticks,
            Map<String, SessionRegistration> factories,
            @Nullable BiFunction<String, String, BackendSession> backend,
            ComponentRegistry components) {
        this.ticks = ticks;
        this.factories = Map.copyOf(factories);
        this.backend = backend;
        this.components = components;
    }

    public TickExecutor getTicks() {
        return ticks;
    }

    public void setWithdraw(Function<String, CompletionStage<Void>> withdraw) {
        this.withdraw = withdraw;
    }

    public CompletableFuture<SessionInventory> create(SessionCommand command) {
        return ticks.submit(
                        () -> {
                            if (!SESSION_ID.matcher(command.getSession().getId()).matches()
                                    || command.getGeneration() <= 0
                                    || command.getOperationId().isEmpty()
                                    || command.getOperationId().length() > 128
                                    || command.getCapacity() < 1
                                    || command.getCapacity() > 128) {
                                throw new IllegalArgumentException("Invalid session command");
                            }
                            var previous = sessions.get(command.getSession().getId());
                            if (previous != null) {
                                if (!previous.command.equals(command))
                                    throw new IllegalArgumentException("Session creation changed");
                                return previous;
                            }
                            if (sessions.size() >= 256)
                                throw new IllegalStateException("Session history full");
                            var factory = factories.get(command.getSessionType());
                            if (factory == null)
                                throw new IllegalArgumentException("Unknown session type");
                            var session = new ManagedSession(command, factory);
                            sessions.put(command.getSession().getId(), session);
                            session.start();
                            return session;
                        })
                .thenCompose(session -> session.ready.thenApply(ignored -> session.inventory()));
    }

    public CompletableFuture<SessionInventory> finish(SessionCommand command) {
        var current = sessions.get(command.getSession().getId());
        if (current != null && current.command.getGeneration() == command.getGeneration())
            current.methodsClosed = true;
        return ticks.submit(
                        () -> {
                            var session = sessions.get(command.getSession().getId());
                            if (session == null
                                    || session.command.getGeneration() != command.getGeneration()) {
                                throw new IllegalArgumentException("Unknown session generation");
                            }
                            return session;
                        })
                .thenCompose(session -> session.finish().thenApply(ignored -> session.inventory()));
    }

    public ManagedSession get(String id, long generation) {
        var session = sessions.get(id);
        if (session == null) throw new IllegalArgumentException("Unknown session");
        if (session.command.getGeneration() != generation)
            throw new IllegalArgumentException("Stale session generation");
        if (session.phase != SessionPhase.SESSION_PHASE_READY)
            throw new IllegalStateException("Session unavailable");
        return session;
    }

    public List<SessionInventory> inventory() {
        return sessions.values().stream().map(ManagedSession::inventory).toList();
    }

    int activeCount() {
        return (int)
                sessions.values().stream()
                        .filter(
                                session ->
                                        switch (session.phase) {
                                            case SESSION_PHASE_STARTING,
                                                    SESSION_PHASE_READY,
                                                    SESSION_PHASE_ENDING ->
                                                    true;
                                            default -> false;
                                        })
                        .count();
    }

    @ApiStatus.Internal
    public final class ManagedSession {
        private final SessionCommand command;
        private final Session behavior;
        private final SessionScope scope;
        private final CompletableFuture<Void> ready = new CompletableFuture<>();
        private final CompletableFuture<Void> ended = new CompletableFuture<>();
        private final Set<Player> joined = Collections.newSetFromMap(new IdentityHashMap<>());
        private volatile SessionPhase phase = SessionPhase.SESSION_PHASE_STARTING;
        private boolean finishing;
        private volatile boolean methodsClosed;
        private @Nullable Throwable creationFailure;

        ManagedSession(SessionCommand command, SessionRegistration registration) {
            this.command = command;
            behavior = registration.create();
            scope =
                    new SessionScope(
                            command.getSession().getId(),
                            command.getGeneration(),
                            ticks,
                            this::finish,
                            registration.backend(command.getSession().getId(), backend),
                            components);
        }

        public SessionCommand getCommand() {
            return command;
        }

        public SessionPhase getPhase() {
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
            if (methodsClosed || phase != SessionPhase.SESSION_PHASE_READY)
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
                                                        phase = SessionPhase.SESSION_PHASE_READY;
                                                    EventDispatcher.call(
                                                            new SessionCreateEvent(scope));
                                                    ready.complete(null);
                                                } else {
                                                    phase = SessionPhase.SESSION_PHASE_FAILED;
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
                                if (phase != SessionPhase.SESSION_PHASE_READY)
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
                                                    EventDispatcher.call(
                                                            new SessionJoinEvent(scope, player));
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
                            EventDispatcher.call(new SessionLeaveEvent(scope, player));
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
                            phase = SessionPhase.SESSION_PHASE_ENDING;
                            // Creation must settle before scoped resources can be disposed.
                            ready.handle((ignored, error) -> null)
                                    .thenCompose(
                                            ignored -> withdraw.apply(command.getSession().getId()))
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
                                                                    var failure =
                                                                            error == null
                                                                                    ? creationFailure
                                                                                    : error;
                                                                    phase =
                                                                            failure == null
                                                                                    ? SessionPhase
                                                                                            .SESSION_PHASE_ENDED
                                                                                    : SessionPhase
                                                                                            .SESSION_PHASE_FAILED;
                                                                    if (failure == null)
                                                                        ended.complete(null);
                                                                    else
                                                                        ended.completeExceptionally(
                                                                                failure);
                                                                } catch (Exception failure) {
                                                                    phase =
                                                                            SessionPhase
                                                                                    .SESSION_PHASE_FAILED;
                                                                    ended.completeExceptionally(
                                                                            failure);
                                                                }
                                                                return null;
                                                            }));
                        }
                        return null;
                    });
            return ended;
        }

        SessionInventory inventory() {
            return SessionInventory.newBuilder()
                    .setSession(command.getSession())
                    .setGeneration(command.getGeneration())
                    .setSessionType(command.getSessionType())
                    .setCapacity(command.getCapacity())
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
