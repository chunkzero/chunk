package dev.chunkzero.runtime;

import dev.chunkzero.backend.client.BackendSession;
import dev.chunkzero.backend.client.OperationId;
import dev.chunkzero.runtime.minestom.event.SessionDestroyEvent;
import dev.chunkzero.runtime.minestom.event.SessionEvent;
import dev.chunkzero.runtime.minestom.internal.ComponentRegistry;

import net.minestom.server.MinecraftServer;
import net.minestom.server.entity.Player;
import net.minestom.server.event.Event;
import net.minestom.server.event.EventDispatcher;
import net.minestom.server.event.EventFilter;
import net.minestom.server.event.EventNode;
import net.minestom.server.event.trait.PlayerEvent;
import net.minestom.server.instance.InstanceContainer;

import org.jetbrains.annotations.NotNull;
import org.jetbrains.annotations.Nullable;

import java.time.Duration;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.IdentityHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.function.Supplier;
import java.util.regex.Pattern;

/** Tick-thread ownership of one session's instances, events and closeable resources. */
public final class SessionScope {
    private static final Pattern ACTION = Pattern.compile("[A-Za-z0-9_-]{1,32}");

    private final String id;
    private final long generation;
    private final TickExecutor ticks;
    private final Supplier<CompletionStage<Void>> requestFinish;
    private final @Nullable BackendSession backend;
    private final ComponentRegistry components;
    private final List<InstanceContainer> ownedInstances = new CopyOnWriteArrayList<>();
    private final List<AutoCloseable> resources = new ArrayList<>();
    private final Map<Player, List<AutoCloseable>> playerResources = new IdentityHashMap<>();
    private final Map<Class<?>, AutoCloseable> sharedResources = new HashMap<>();
    private final Set<Player> players = ConcurrentHashMap.newKeySet();
    private final EventNode<Event> events;
    private boolean disposed;

    SessionScope(
            String id,
            long generation,
            TickExecutor ticks,
            Supplier<CompletionStage<Void>> requestFinish,
            @Nullable BackendSession backend) {
        this(id, generation, ticks, requestFinish, backend, new ComponentRegistry(List.of()));
    }

    SessionScope(
            String id,
            long generation,
            TickExecutor ticks,
            Supplier<CompletionStage<Void>> requestFinish,
            @Nullable BackendSession backend,
            ComponentRegistry components) {
        this.id = id;
        this.generation = generation;
        this.ticks = ticks;
        this.requestFinish = requestFinish;
        this.backend = backend;
        this.components = components;
        events =
                EventNode.event(
                        "session-" + id + "-" + generation,
                        EventFilter.ALL,
                        event ->
                                event instanceof SessionEvent lifecycle
                                        ? lifecycle.getSession() == this
                                        : event instanceof PlayerEvent player
                                                && players.contains(player.getPlayer()));
        MinecraftServer.getGlobalEventHandler().addChild(events);
        if (backend != null) resources.add(backend);
    }

    public String getId() {
        return id;
    }

    public long getGeneration() {
        return generation;
    }

    public @Nullable BackendSession getBackend() {
        return backend;
    }

    /** Receives this scope's lifecycle notifications and its admitted players' Minestom events. */
    public @NotNull EventNode<Event> getEvents() {
        return events;
    }

    public List<InstanceContainer> getInstances() {
        return List.copyOf(ownedInstances);
    }

    TickExecutor getTicks() {
        return ticks;
    }

    Set<Player> getPlayers() {
        return players;
    }

    public InstanceContainer createInstance() {
        ticks.checkThread();
        checkActive();
        var instance = MinecraftServer.getInstanceManager().createInstanceContainer();
        ownedInstances.add(instance);
        return instance;
    }

    /** Register subscriptions and other session resources for disposal. */
    public <T extends AutoCloseable> T own(T resource) {
        ticks.checkThread();
        if (disposed) reject(resource, "Session disposed");
        resources.add(resource);
        return resource;
    }

    /** Player resources are keyed by the admitted object, keeping replacements independent. */
    public <T extends AutoCloseable> T own(Player player, T resource) {
        ticks.checkThread();
        if (disposed || !players.contains(player)) {
            reject(resource, "Player scope unavailable");
        }
        playerResources.computeIfAbsent(player, ignored -> new ArrayList<>()).add(resource);
        return resource;
    }

    /** Return one owned resource per class, creating it on first access on the tick thread. */
    public <T extends AutoCloseable> T resource(Class<T> type, Supplier<T> factory) {
        ticks.checkThread();
        checkActive();
        var existing = sharedResources.get(type);
        if (existing != null) return type.cast(existing);
        var resource = own(factory.get());
        sharedResources.put(type, resource);
        return resource;
    }

    /**
     * Resolves an exact declared component type on the tick thread. Session factories create one
     * instance per scope; process factories share one instance across this app's sessions.
     * Closeable results are owned automatically, including rollback of a failed factory graph.
     */
    public <T> T component(Class<T> type) {
        return resource(ComponentRegistry.SessionComponents.class, () -> components.session(this))
                .get(type);
    }

    void releasePlayer(Player player) throws Exception {
        ticks.checkThread();
        var owned = playerResources.remove(player);
        if (owned != null) closeResources(owned);
    }

    public <T> CompletableFuture<T> onTick(Supplier<T> action) {
        return ticks.submit(
                () -> {
                    checkActive();
                    return action.get();
                });
    }

    public CompletableFuture<Void> onTick(Runnable action) {
        return onTick(
                () -> {
                    action.run();
                    return null;
                });
    }

    /** Stable mutation identity for one action on this exact player delivery. */
    public OperationId operationId(Player player, String action) {
        if (!ACTION.matcher(action).matches())
            throw new IllegalArgumentException("Invalid operation action");
        if (!players.contains(player)) throw new IllegalArgumentException("Player is not admitted");
        var binding = ((ManagedPlayer) player).getBinding();
        return new OperationId(
                id + "/" + player.getUuid() + "/" + binding.getOwnerGeneration() + "/" + action);
    }

    public CompletionStage<Void> finish() {
        return requestFinish.get();
    }

    public AutoCloseable repeatEvery(Duration interval, Runnable action) {
        ticks.checkThread();
        if (interval.isNegative() || interval.isZero())
            throw new IllegalArgumentException("Interval must be positive");
        var task =
                MinecraftServer.getSchedulerManager()
                        .buildTask(
                                () -> {
                                    if (!disposed) action.run();
                                })
                        .repeat(interval)
                        .schedule();
        return own(task::cancel);
    }

    void dispose() throws Exception {
        ticks.checkThread();
        if (disposed) return;
        if (!players.isEmpty()) throw new IllegalStateException("Session still has players");
        disposed = true;
        try {
            try {
                closeResources(resources);
            } finally {
                ownedInstances.forEach(MinecraftServer.getInstanceManager()::unregisterInstance);
            }
        } finally {
            try {
                EventDispatcher.call(new SessionDestroyEvent(this));
            } finally {
                MinecraftServer.getGlobalEventHandler().removeChild(events);
            }
        }
    }

    private void checkActive() {
        if (disposed) throw new IllegalStateException("Session disposed");
    }

    private static void reject(AutoCloseable resource, String message) {
        var failure = new IllegalStateException(message);
        try {
            resource.close();
        } catch (Exception error) {
            failure.addSuppressed(error);
        }
        throw failure;
    }

    private static void closeResources(List<AutoCloseable> owned) throws Exception {
        Exception failure = null;
        for (var resource : owned.reversed()) {
            try {
                resource.close();
            } catch (Exception error) {
                failure = error;
            }
        }
        if (failure != null) throw failure;
    }
}
