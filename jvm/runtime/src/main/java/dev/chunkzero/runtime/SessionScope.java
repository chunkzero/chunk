package dev.chunkzero.runtime;

import dev.chunkzero.backend.client.BackendSession;
import dev.chunkzero.backend.client.OperationId;
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
import net.minestom.server.MinecraftServer;
import net.minestom.server.entity.Player;
import net.minestom.server.event.EventFilter;
import net.minestom.server.event.EventNode;
import net.minestom.server.event.trait.PlayerEvent;
import net.minestom.server.instance.InstanceContainer;
import org.jetbrains.annotations.Nullable;

/** Tick-thread ownership of one session's instances, events and closeable resources. */
public final class SessionScope {
    private static final Pattern ACTION = Pattern.compile("[A-Za-z0-9_-]{1,32}");

    private final String id;
    private final long generation;
    private final TickExecutor ticks;
    private final Supplier<CompletionStage<Void>> requestFinish;
    private final @Nullable BackendSession backend;
    private final List<InstanceContainer> ownedInstances = new CopyOnWriteArrayList<>();
    private final List<AutoCloseable> resources = new ArrayList<>();
    private final Map<Player, List<AutoCloseable>> playerResources = new IdentityHashMap<>();
    private final Map<Class<?>, AutoCloseable> sharedResources = new HashMap<>();
    private final Set<Player> players = ConcurrentHashMap.newKeySet();
    private final EventNode<PlayerEvent> events;
    private boolean disposed;

    SessionScope(
            String id,
            long generation,
            TickExecutor ticks,
            Supplier<CompletionStage<Void>> requestFinish,
            @Nullable BackendSession backend) {
        this.id = id;
        this.generation = generation;
        this.ticks = ticks;
        this.requestFinish = requestFinish;
        this.backend = backend;
        events = EventNode.value("session-" + id + "-" + generation, EventFilter.PLAYER, players::contains);
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

    public EventNode<PlayerEvent> getEvents() {
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
        if (ownedInstances.size() >= 16) throw new IllegalStateException("Session instance limit reached");
        var instance = MinecraftServer.getInstanceManager().createInstanceContainer();
        ownedInstances.add(instance);
        return instance;
    }

    /** Register subscriptions and other session resources for disposal. */
    public <T extends AutoCloseable> T own(T resource) {
        ticks.checkThread();
        if (disposed) reject(resource, "Session disposed");
        if (resourceCount() >= 1024) reject(resource, "Session resource limit reached");
        resources.add(resource);
        return resource;
    }

    /** Player resources are keyed by the admitted object, keeping replacements independent. */
    public <T extends AutoCloseable> T own(Player player, T resource) {
        ticks.checkThread();
        if (disposed || !players.contains(player) || resourceCount() >= 1024) {
            reject(resource, "Player scope unavailable");
        }
        playerResources.computeIfAbsent(player, ignored -> new ArrayList<>()).add(resource);
        return resource;
    }

    <T extends AutoCloseable> T resource(Class<T> type, Supplier<T> factory) {
        ticks.checkThread();
        checkActive();
        var existing = sharedResources.get(type);
        if (existing != null) return type.cast(existing);
        var resource = own(factory.get());
        sharedResources.put(type, resource);
        return resource;
    }

    void releasePlayer(Player player) throws Exception {
        ticks.checkThread();
        var owned = playerResources.remove(player);
        if (owned != null) closeResources(owned);
    }

    public <T> CompletableFuture<T> onTick(Supplier<T> action) {
        return ticks.submit(() -> {
            checkActive();
            return action.get();
        });
    }

    public CompletableFuture<Void> onTick(Runnable action) {
        return onTick(() -> {
            action.run();
            return null;
        });
    }

    /** Stable mutation identity for one action on this exact player delivery. */
    public OperationId operationId(Player player, String action) {
        if (!ACTION.matcher(action).matches()) throw new IllegalArgumentException("Invalid operation action");
        if (!players.contains(player)) throw new IllegalArgumentException("Player is not admitted");
        var binding = ((ManagedPlayer) player).getBinding();
        return new OperationId(id + "/" + player.getUuid() + "/" + binding.getOwnerGeneration() + "/" + action);
    }

    public CompletionStage<Void> finish() {
        return requestFinish.get();
    }

    public AutoCloseable repeatEvery(Duration interval, Runnable action) {
        ticks.checkThread();
        if (interval.isNegative() || interval.isZero()) throw new IllegalArgumentException("Interval must be positive");
        var task = MinecraftServer.getSchedulerManager().buildTask(() -> {
            if (!disposed) action.run();
        }).repeat(interval).schedule();
        return own(task::cancel);
    }

    void dispose() throws Exception {
        ticks.checkThread();
        if (disposed) return;
        if (!players.isEmpty()) throw new IllegalStateException("Session still has players");
        disposed = true;
        MinecraftServer.getGlobalEventHandler().removeChild(events);
        try {
            closeResources(resources);
        } finally {
            ownedInstances.forEach(MinecraftServer.getInstanceManager()::unregisterInstance);
        }
    }

    private void checkActive() {
        if (disposed) throw new IllegalStateException("Session disposed");
    }

    private int resourceCount() {
        return resources.size() + playerResources.values().stream().mapToInt(List::size).sum();
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
