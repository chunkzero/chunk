package dev.chunkzero.runtime.minestom.internal;

import chunk.sync.v1.Jvm.JvmDelivery;
import chunk.sync.v1.Jvm.JvmDeliveryPhase;
import chunk.sync.v1.Jvm.JvmDeliveryStatus;
import chunk.sync.v1.Jvm.JvmSessionPhase;
import chunk.sync.v1.Jvm.PlayerSetup;

import dev.chunkzero.runtime.ManagedPlayer;
import dev.chunkzero.runtime.SessionManager;

import net.kyori.adventure.text.Component;
import net.minestom.server.coordinate.Pos;
import net.minestom.server.event.Event;
import net.minestom.server.event.EventNode;
import net.minestom.server.event.player.AsyncPlayerConfigurationEvent;
import net.minestom.server.event.player.AsyncPlayerPreLoginEvent;

import org.jetbrains.annotations.ApiStatus;

import java.util.ArrayList;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import java.util.function.BooleanSupplier;
import java.util.function.LongSupplier;
import java.util.regex.Pattern;

/**
 * Runs the deliveries core's topic lists. It prepares each once its session is ready, admits its
 * player through the {@code chunk:delivery} login plugin, and closes it once it is withdrawn or its
 * key is gone. A delivery it can't prepare, or that is withdrawn first, is closed at once.
 */
@ApiStatus.Internal
public final class GameplayService {
    private static final Pattern USERNAME = Pattern.compile("[A-Za-z0-9_]{1,16}");
    private static final long WAIT_NANOS = TimeUnit.SECONDS.toNanos(30);

    private final SessionManager manager;
    private final LongSupplier now;
    private final BooleanSupplier ready;
    private final DeliveryFence owners = new DeliveryFence();
    private final EventNode<Event> events = EventNode.all("gameplay-delivery");
    // Guarded by itself, like the maps after it: the topic's latest deliveries, and those held.
    private final Map<String, PreparedDelivery> preparations = new LinkedHashMap<>();
    private Map<String, JvmDelivery> wanted = Map.of();
    // Deliveries waiting for their session, since when, and those closed without preparing.
    private final Map<String, Long> pending = new HashMap<>();
    private final Map<String, JvmDelivery> refused = new HashMap<>();

    public GameplayService(SessionManager manager, LongSupplier now, BooleanSupplier ready) {
        this.manager = manager;
        this.now = now;
        this.ready = ready;
        manager.setWithdraw(
                id -> {
                    synchronized (preparations) {
                        var closing =
                                preparations.values().stream()
                                        .filter(
                                                prepared ->
                                                        prepared.getDelivery()
                                                                .getSession()
                                                                .equals(id))
                                        .map(PreparedDelivery::close)
                                        .toArray(CompletableFuture<?>[]::new);
                        return CompletableFuture.allOf(closing);
                    }
                });
        events.addListener(AsyncPlayerPreLoginEvent.class, this::preLogin);
        events.addListener(AsyncPlayerConfigurationEvent.class, this::configure);
        manager.getProcess().eventHandler().addChild(events);
    }

    private void preLogin(AsyncPlayerPreLoginEvent event) {
        try {
            var payload =
                    event.sendPluginRequest("chunk:delivery", new byte[0])
                            .get(5, TimeUnit.SECONDS)
                            .payload();
            if (payload == null || payload.length > 4096)
                throw new IllegalArgumentException("Invalid delivery setup");
            var setup = PlayerSetup.parseFrom(payload);
            synchronized (preparations) {
                var prepared = preparations.get(setup.getOperationId());
                if (prepared == null) throw new IllegalArgumentException("Unknown operation");
                event.setGameProfile(
                        prepared.consume(setup, event.getGameProfile(), event.getConnection()));
            }
        } catch (Exception ignored) {
            event.getConnection().kick(Component.text("Delivery rejected"));
        }
    }

    private void configure(AsyncPlayerConfigurationEvent event) {
        try {
            PreparedDelivery prepared;
            synchronized (preparations) {
                prepared =
                        preparations.values().stream()
                                .filter(
                                        candidate ->
                                                candidate.owns(
                                                        event.getPlayer().getPlayerConnection()))
                                .findFirst()
                                .orElseThrow(
                                        () -> new IllegalArgumentException("Unknown delivery"));
            }
            event.setSpawningInstance(prepared.configure((ManagedPlayer) event.getPlayer()));
            event.getPlayer().setRespawnPoint(new Pos(0.5, 42, 0.5));
        } catch (Exception ignored) {
            event.getPlayer().kick(Component.text("Session unavailable"));
        }
    }

    /** Runs the topic's latest deliveries, keyed by operation ID. */
    public void apply(Map<String, JvmDelivery> deliveries) {
        synchronized (preparations) {
            wanted = Map.copyOf(deliveries);
            preparations.forEach(
                    (operation, prepared) -> {
                        var delivery = wanted.get(operation);
                        if (delivery == null || delivery.getWithdraw()) prepared.close();
                    });
            refused.keySet().retainAll(wanted.keySet());
            pending.keySet().retainAll(wanted.keySet());
            wanted.forEach(
                    (operation, delivery) -> {
                        if (!preparations.containsKey(operation) && !refused.containsKey(operation))
                            pending.putIfAbsent(operation, now.getAsLong());
                    });
            settle();
        }
    }

    /** Prepares waiting deliveries whose session became ready, and closes expired ones. */
    public void flush() {
        synchronized (preparations) {
            preparations.values().forEach(PreparedDelivery::checkDeadline);
            settle();
        }
    }

    /** Prepares or refuses each waiting delivery it can, and forgets those closed and gone. */
    private void settle() {
        for (var operation : List.copyOf(pending.keySet())) {
            var delivery = wanted.get(operation);
            var phase = manager.phase(delivery.getSession());
            if (phase == JvmSessionPhase.JVM_SESSION_PHASE_READY && !delivery.getWithdraw()) {
                try {
                    prepare(operation, delivery);
                } catch (RuntimeException error) {
                    refused.put(operation, delivery);
                }
            } else if (delivery.getWithdraw()
                    || !ready.getAsBoolean()
                    || (phase != null && phase != JvmSessionPhase.JVM_SESSION_PHASE_STARTING)
                    || now.getAsLong() - pending.get(operation) >= WAIT_NANOS) {
                refused.put(operation, delivery);
            } else continue;
            pending.remove(operation);
        }
        preparations
                .entrySet()
                .removeIf(
                        entry ->
                                !wanted.containsKey(entry.getKey())
                                        && entry.getValue().isReleased());
    }

    private void prepare(String operation, JvmDelivery delivery) {
        if (!ready.getAsBoolean()) throw new IllegalStateException("Server not ready");
        var identity = delivery.getPlayer();
        if (!USERNAME.matcher(identity.getUsername()).matches()
                || !UUID.fromString(identity.getUuid()).toString().equals(identity.getUuid())) {
            throw new IllegalArgumentException("Invalid player identity");
        }
        if (preparations.size() >= 4096)
            throw new IllegalStateException("Process delivery capacity reached");
        var session = manager.get(delivery.getSession());
        var reserved =
                preparations.values().stream()
                        .filter(
                                candidate ->
                                        candidate
                                                        .getDelivery()
                                                        .getSession()
                                                        .equals(delivery.getSession())
                                                && !candidate.isReleased())
                        .count();
        if (reserved >= session.getCapacity()) throw new IllegalStateException("Session full");
        preparations.put(
                operation,
                new PreparedDelivery(
                        operation, delivery, owners, now, session, manager.getTicks()));
    }

    /** Whether the player of delivery {@code operation} is in session {@code session}. */
    public boolean arrived(String operation, String session) {
        synchronized (preparations) {
            var prepared = preparations.get(operation);
            return prepared != null && prepared.arrivedIn(session);
        }
    }

    public List<JvmDeliveryStatus> deliveries() {
        return deliveries(new HashMap<>());
    }

    /**
     * Every delivery it holds, including those closed whose key remains, counting those PREPARED in
     * each session into {@code prepared}.
     */
    public List<JvmDeliveryStatus> deliveries(Map<String, Integer> prepared) {
        synchronized (preparations) {
            var statuses = new ArrayList<JvmDeliveryStatus>();
            preparations
                    .values()
                    .forEach(
                            delivery -> {
                                var status = delivery.status();
                                statuses.add(status);
                                if (status.getPhase()
                                        == JvmDeliveryPhase.JVM_DELIVERY_PHASE_PREPARED)
                                    prepared.merge(
                                            delivery.getDelivery().getSession(), 1, Integer::sum);
                            });
            refused.forEach(
                    (operation, delivery) ->
                            statuses.add(
                                    JvmDeliveryStatus.newBuilder()
                                            .setOperationId(operation)
                                            .setGeneration(delivery.getGeneration())
                                            .setPhase(JvmDeliveryPhase.JVM_DELIVERY_PHASE_CLOSED)
                                            .build()));
            return statuses;
        }
    }

    public void close() {
        manager.getProcess().eventHandler().removeChild(events);
        synchronized (preparations) {
            preparations.values().forEach(PreparedDelivery::close);
        }
    }
}
