package dev.chunkzero.runtime;

import dev.chunkzero.backend.client.BackendSession;
import dev.chunkzero.backend.client.OperationId;
import dev.chunkzero.runtime.minestom.event.SessionDestroyEvent;
import dev.chunkzero.runtime.minestom.event.SessionEvent;
import dev.chunkzero.runtime.minestom.internal.ComponentRegistry;

import net.minestom.server.ServerProcess;
import net.minestom.server.entity.Player;
import net.minestom.server.event.Event;
import net.minestom.server.event.EventFilter;
import net.minestom.server.event.EventNode;
import net.minestom.server.event.trait.EntityEvent;
import net.minestom.server.event.trait.InstanceEvent;
import net.minestom.server.event.trait.PlayerEvent;
import net.minestom.server.instance.ChunkLoader;
import net.minestom.server.instance.Instance;
import net.minestom.server.instance.InstanceContainer;
import net.minestom.server.registry.RegistryKey;
import net.minestom.server.timer.ExecutionType;
import net.minestom.server.timer.Scheduler;
import net.minestom.server.timer.Task;
import net.minestom.server.timer.TaskSchedule;
import net.minestom.server.world.DimensionType;

import org.jetbrains.annotations.NotNull;
import org.jetbrains.annotations.Nullable;

import java.time.Duration;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.IdentityHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.function.Supplier;
import java.util.regex.Pattern;

/**
 * The gameplay isolate of one session within the shared server process. It owns the session's
 * instances with their entities and schedulers, its event node, scheduler, and closeable resources
 * until disposal. It is not a memory or failure boundary.
 */
public final class SessionScope {
    private static final Pattern ACTION = Pattern.compile("[A-Za-z0-9_-]{1,32}");

    private final ServerProcess process;
    private final String id;
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
    private final Scheduler scheduler;
    private final List<Task> drivers;
    private boolean disposed;

    SessionScope(
            ServerProcess process,
            String id,
            TickExecutor ticks,
            Supplier<CompletionStage<Void>> requestFinish,
            @Nullable BackendSession backend) {
        this(process, id, ticks, requestFinish, backend, new ComponentRegistry(List.of()));
    }

    SessionScope(
            ServerProcess process,
            String id,
            TickExecutor ticks,
            Supplier<CompletionStage<Void>> requestFinish,
            @Nullable BackendSession backend,
            ComponentRegistry components) {
        this.process = process;
        this.id = id;
        this.ticks = ticks;
        this.requestFinish = requestFinish;
        this.backend = backend;
        this.components = components;
        events = EventNode.event("session-" + id, EventFilter.ALL, this::owns);
        try {
            var schedulers = process.schedulerManager();
            scheduler = schedulers.createScheduler();
            drivers =
                    List.of(
                            schedulers
                                    .buildTask(scheduler::processTick)
                                    .repeat(TaskSchedule.nextTick())
                                    .schedule(),
                            schedulers
                                    .buildTask(scheduler::processTickEnd)
                                    .repeat(TaskSchedule.nextTick())
                                    .executionType(ExecutionType.TICK_END)
                                    .schedule());
            process.eventHandler().addChild(events);
        } catch (RuntimeException | Error failure) {
            if (backend != null) backend.close();
            throw failure;
        }
        if (backend != null) resources.add(backend);
    }

    public String getId() {
        return id;
    }

    public @Nullable BackendSession getBackend() {
        return backend;
    }

    /** Minestom process shared by this app's sessions; it hosts this scope's owned resources. */
    public ServerProcess getProcess() {
        return process;
    }

    /**
     * Receives this scope's lifecycle notifications, its admitted players' events, and events of
     * its instances and the entities in them.
     */
    public @NotNull EventNode<Event> getEvents() {
        return events;
    }

    /** Session-owned scheduler run on the process tick. Disposal cancels its tasks. */
    public Scheduler getScheduler() {
        return scheduler;
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
        return createInstance(DimensionType.OVERWORLD, null);
    }

    /** Disposal unregisters the instance, removing its entities and closing its scheduler. */
    public InstanceContainer createInstance(
            RegistryKey<DimensionType> dimension, @Nullable ChunkLoader loader) {
        checkThread();
        checkActive();
        var instance =
                new InstanceContainer(
                        process, UUID.randomUUID(), dimension, loader, dimension.key());
        // Owned before registration so the scope receives InstanceRegisterEvent.
        ownedInstances.add(instance);
        try {
            process.instanceManager().registerInstance(instance);
        } catch (Throwable failure) {
            ownedInstances.remove(instance);
            throw failure;
        }
        return instance;
    }

    /** Register subscriptions and other session resources for disposal. */
    public <T extends AutoCloseable> T own(T resource) {
        checkThread();
        if (disposed) reject(resource, "Session disposed");
        resources.add(resource);
        return resource;
    }

    /** Player resources are keyed by the admitted object, keeping replacements independent. */
    public <T extends AutoCloseable> T own(Player player, T resource) {
        checkThread();
        if (disposed || !players.contains(player)) {
            reject(resource, "Player scope unavailable");
        }
        playerResources.computeIfAbsent(player, ignored -> new ArrayList<>()).add(resource);
        return resource;
    }

    /** Return one owned resource per class, creating it on first access on the tick thread. */
    public <T extends AutoCloseable> T resource(Class<T> type, Supplier<T> factory) {
        checkThread();
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
        checkThread();
        var owned = playerResources.remove(player);
        if (owned != null) closeAll(owned.reversed());
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
        var generation = ((ManagedPlayer) player).getBinding().getGeneration();
        return new OperationId(
                id
                        + "/"
                        + player.getUuid()
                        + "/"
                        + generation.getEpoch()
                        + "."
                        + generation.getRevision()
                        + "/"
                        + action);
    }

    public CompletionStage<Void> finish() {
        return requestFinish.get();
    }

    public AutoCloseable repeatEvery(Duration interval, Runnable action) {
        checkThread();
        checkActive();
        if (interval.isNegative() || interval.isZero())
            throw new IllegalArgumentException("Interval must be positive");
        return scheduler.buildTask(action).repeat(interval).schedule()::cancel;
    }

    /**
     * Session code runs on the process tick thread, or on the process's only dispatcher thread,
     * which ticks instances and entities serially within the same tick.
     */
    private boolean isTickThread() {
        if (ticks.isCurrentThread()) return true;
        var dispatchers = process.dispatcher().threads();
        return dispatchers.size() == 1 && dispatchers.getFirst() == Thread.currentThread();
    }

    void dispose() throws Exception {
        checkThread();
        if (disposed) return;
        if (!players.isEmpty()) throw new IllegalStateException("Session still has players");
        disposed = true;
        closeAll(
                List.of(
                        () -> {
                            drivers.forEach(Task::cancel);
                            scheduler.close();
                        },
                        () -> closeAll(resources.reversed()),
                        this::unregisterInstances,
                        () -> process.eventHandler().call(new SessionDestroyEvent(this)),
                        () -> process.eventHandler().removeChild(events)));
    }

    private void unregisterInstances() throws Exception {
        var instances = process.instanceManager();
        closeAll(
                ownedInstances.stream()
                        .filter(Instance::isRegistered)
                        .<AutoCloseable>map(
                                instance -> () -> instances.unregisterInstance(instance))
                        .toList());
    }

    private boolean owns(Event event) {
        return switch (event) {
            case SessionEvent lifecycle -> lifecycle.getSession() == this;
            case PlayerEvent player -> players.contains(player.getPlayer());
            case InstanceEvent instance -> ownedInstances.contains(instance.getInstance());
            case EntityEvent entity -> ownedInstances.contains(entity.getEntity().getInstance());
            default -> false;
        };
    }

    private void checkThread() {
        if (!isTickThread())
            throw new IllegalStateException("Use SessionScope.onTick for world changes");
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

    /**
     * Closes every step in order, even after errors; later failures are suppressed into the first.
     */
    private static void closeAll(List<? extends AutoCloseable> steps) throws Exception {
        Throwable failure = null;
        for (var step : steps) {
            try {
                step.close();
            } catch (Throwable error) {
                if (failure == null) failure = error;
                else failure.addSuppressed(error);
            }
        }
        switch (failure) {
            case null -> {}
            case Exception exception -> throw exception;
            case Error error -> throw error;
            default -> throw new IllegalStateException(failure);
        }
    }
}
