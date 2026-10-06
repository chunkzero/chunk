package com.chunkzero.chunk.multistom;

import com.chunkzero.chunk.multistom.event.SessionCreateEvent;
import com.chunkzero.chunk.multistom.event.SessionJoinEvent;
import com.chunkzero.chunk.multistom.event.SessionLeaveEvent;
import com.chunkzero.chunk.multistom.internal.ComponentRegistry;
import com.chunkzero.chunk.runtime.Delivery;
import com.chunkzero.chunk.runtime.SessionControl;
import com.chunkzero.chunk.runtime.SessionHandler;
import com.chunkzero.chunk.runtime.SessionMethod;

import net.minestom.server.ServerProcess;
import net.minestom.server.entity.Player;

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
import java.util.function.Function;
import java.util.function.Supplier;
import java.util.stream.Collectors;

/**
 * Runs each session as a {@link Session} with its own {@link SessionScope} in the app's shared
 * Minestom process. Hooks start on the tick thread.
 */
@ApiStatus.Internal
public final class SessionManager implements SessionHandler {
    private static final System.Logger LOG = System.getLogger(SessionManager.class.getName());
    private final ServerProcess process;
    private final TickExecutor ticks;
    private final Map<String, SessionRegistration> factories;
    private final ComponentRegistry components;
    private final Map<String, SessionMethodBinding<?, ?>> methods;
    private final Map<String, ManagedSession> sessions = new ConcurrentHashMap<>();

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
                Map.of());
    }

    SessionManager(
            ServerProcess process,
            TickExecutor ticks,
            Map<String, SessionRegistration> factories,
            Map<String, SessionMethodBinding<?, ?>> methods) {
        this(process, ticks, factories, new ComponentRegistry(List.of()), methods);
    }

    SessionManager(
            ServerProcess process,
            TickExecutor ticks,
            Map<String, SessionRegistration> factories,
            ComponentRegistry components,
            Map<String, SessionMethodBinding<?, ?>> methods) {
        this.process = process;
        this.ticks = ticks;
        this.factories = Map.copyOf(factories);
        this.components = components;
        this.methods = Map.copyOf(methods);
    }

    public TickExecutor getTicks() {
        return ticks;
    }

    public ServerProcess getProcess() {
        return process;
    }

    @Override
    public void create(SessionControl control) {
        ticks.submit(
                        () -> {
                            var registration = factories.get(control.type());
                            if (registration == null)
                                throw new IllegalArgumentException("Unknown session type");
                            var managed =
                                    new ManagedSession(
                                            control,
                                            registration.create(
                                                    control.capacity(),
                                                    control.configurationJson()));
                            sessions.put(control.id(), managed);
                            managed.start();
                            return null;
                        })
                .exceptionally(
                        error -> {
                            control.fail(error);
                            return null;
                        });
    }

    @Override
    public void finish(SessionControl control) {
        ticks.submit(
                        () -> {
                            var managed = sessions.get(control.id());
                            if (managed == null) control.ended();
                            else managed.finish();
                            return null;
                        })
                .exceptionally(
                        error -> {
                            control.ended(error);
                            return null;
                        });
    }

    @Override
    public CompletionStage<String> method(SessionControl control, SessionMethod call) {
        var binding = methods.get(control.type() + "/" + call.name());
        if (binding == null)
            return CompletableFuture.failedFuture(
                    new IllegalArgumentException("Undeclared session method"));
        return ticks.submit(
                () -> {
                    var managed = sessions.get(control.id());
                    if (managed != null && managed.isDisconnected(call.delivery()))
                        call.delivery().left();
                    if (!call.start()) return null;
                    if (managed == null || !control.isReady())
                        throw new IllegalStateException("Session unavailable");
                    return binding.invoke(managed.behavior, call.argumentsJson());
                });
    }

    /** The ready session {@code id}. */
    public ManagedSession get(String id) {
        var session = sessions.get(id);
        if (session == null) throw new IllegalArgumentException("Unknown session");
        if (!session.control.isReady()) throw new IllegalStateException("Session unavailable");
        return session;
    }

    @ApiStatus.Internal
    public final class ManagedSession {
        private final SessionControl control;
        private final Session behavior;
        private final SessionScope scope;
        private final Set<Player> joined = Collections.newSetFromMap(new IdentityHashMap<>());
        private boolean finishing;

        ManagedSession(SessionControl control, Session behavior) {
            this.control = control;
            this.behavior = behavior;
            scope =
                    new SessionScope(
                            process,
                            control.id(),
                            ticks,
                            control::finish,
                            control.backend(),
                            components);
        }

        public String getSessionType() {
            return control.type();
        }

        public int getCapacity() {
            return control.capacity();
        }

        public SessionScope getScope() {
            return scope;
        }

        boolean isDisconnected(Delivery delivery) {
            return scope.getPlayers().stream()
                    .anyMatch(
                            player ->
                                    ((ManagedPlayer) player).getDelivery() == delivery
                                            && !player.getPlayerConnection().isOnline());
        }

        void start() {
            invoke(() -> behavior.onCreate(scope))
                    .whenComplete(
                            (ignored, error) ->
                                    ticks.submit(
                                            () -> {
                                                // A creation past its deadline already ended.
                                                if (finishing) return null;
                                                if (error == null
                                                        && !scope.getInstances().isEmpty()) {
                                                    if (control.ready())
                                                        process.eventHandler()
                                                                .call(
                                                                        new SessionCreateEvent(
                                                                                scope));
                                                } else {
                                                    control.fail(
                                                            error == null
                                                                    ? new IllegalStateException(
                                                                            "Session has no"
                                                                                    + " instances")
                                                                    : error);
                                                }
                                                return null;
                                            }));
        }

        public CompletableFuture<Void> join(Player player) {
            return ticks.submit(
                            () -> {
                                if (!control.isReady())
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

        /** Runs {@code onFinish} and disposes the scope, on the tick thread. */
        void finish() {
            finishing = true;
            invoke(behavior::onFinish)
                    .whenComplete(
                            (ignored, error) ->
                                    ticks.submit(
                                            () -> {
                                                try {
                                                    scope.dispose();
                                                } catch (Exception failure) {
                                                    // An unclosed resource keeps the session
                                                    // ending and counted.
                                                    LOG.log(
                                                            System.Logger.Level.ERROR,
                                                            "Session "
                                                                    + control.id()
                                                                    + " failed to dispose",
                                                            failure);
                                                    return null;
                                                }
                                                sessions.remove(control.id(), this);
                                                if (error == null) control.ended();
                                                else control.ended(error);
                                                return null;
                                            }));
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
