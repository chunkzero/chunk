package dev.chunkzero.runtime;

import chunk.v1.Common.DeploymentRef;
import chunk.v1.GameplayGrpc;
import chunk.v1.GameplayOuterClass.ConfigurationRequest;
import chunk.v1.GameplayOuterClass.ConfigurationResponse;
import chunk.v1.GameplayOuterClass.PlayerDelivery;
import chunk.v1.GameplayOuterClass.PlayerPreparation;
import chunk.v1.GameplayOuterClass.PlayerSetup;
import chunk.v1.GameplayOuterClass.PlayerWithdrawal;
import chunk.v1.Supervision.DeliveryInventory;
import chunk.v1.Supervision.DeliveryPhase;
import chunk.v1.Supervision.SessionInventory;

import io.grpc.Status;
import io.grpc.stub.StreamObserver;

import net.kyori.adventure.text.Component;
import net.minestom.server.MinecraftServer;
import net.minestom.server.coordinate.Pos;
import net.minestom.server.event.Event;
import net.minestom.server.event.EventNode;
import net.minestom.server.event.player.AsyncPlayerConfigurationEvent;
import net.minestom.server.event.player.AsyncPlayerPreLoginEvent;

import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import java.util.function.LongSupplier;
import java.util.regex.Pattern;

final class GameplayService extends GameplayGrpc.GameplayImplBase {
    private static final Pattern USERNAME = Pattern.compile("[A-Za-z0-9_]{1,16}");

    private final DeploymentRef deployment;
    private final long generation;
    private final SessionManager manager;
    private final LongSupplier now;
    private final String runtimeId;
    private final Map<String, PreparedDelivery> preparations = new LinkedHashMap<>();
    private final DeliveryFence owners = new DeliveryFence();
    private final ConfigurationResponse configurationArtifact;
    private final EventNode<Event> events = EventNode.all("gameplay-delivery");
    private String endpoint = "";

    GameplayService(DeploymentRef deployment, long generation, SessionManager manager) {
        this(deployment, generation, manager, System::nanoTime, "bridge");
    }

    GameplayService(
            DeploymentRef deployment, long generation, SessionManager manager, LongSupplier now) {
        this(deployment, generation, manager, now, "bridge");
    }

    GameplayService(
            DeploymentRef deployment,
            long generation,
            SessionManager manager,
            LongSupplier now,
            String runtimeId) {
        this.deployment = deployment;
        this.generation = generation;
        this.manager = manager;
        this.now = now;
        this.runtimeId = runtimeId;
        configurationArtifact =
                ConfigurationResponse.newBuilder()
                        .setDeployment(deployment)
                        .setProcessGeneration(generation)
                        .setRuntimeId(runtimeId)
                        .setProtocol(MinecraftServer.PROTOCOL_VERSION)
                        .build();
        manager.setWithdraw(
                id -> {
                    synchronized (preparations) {
                        var closing =
                                preparations.values().stream()
                                        .filter(
                                                prepared ->
                                                        prepared.getDelivery()
                                                                .getSession()
                                                                .getId()
                                                                .equals(id))
                                        .map(PreparedDelivery::close)
                                        .toArray(CompletableFuture<?>[]::new);
                        return CompletableFuture.allOf(closing);
                    }
                });
        events.addListener(AsyncPlayerPreLoginEvent.class, this::preLogin);
        events.addListener(AsyncPlayerConfigurationEvent.class, this::configure);
        MinecraftServer.getGlobalEventHandler().addChild(events);
    }

    ConfigurationResponse getConfigurationArtifact() {
        return configurationArtifact;
    }

    String getEndpoint() {
        return endpoint;
    }

    void setEndpoint(String endpoint) {
        this.endpoint = endpoint;
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

    @Override
    public void configuration(
            ConfigurationRequest request, StreamObserver<ConfigurationResponse> response) {
        if (!request.getDeployment().equals(deployment)) {
            response.onError(
                    Status.PERMISSION_DENIED
                            .withDescription("Deployment mismatch")
                            .asRuntimeException());
            return;
        }
        response.onNext(configurationArtifact);
        response.onCompleted();
    }

    @Override
    public void preparePlayer(PlayerDelivery request, StreamObserver<PlayerPreparation> response) {
        try {
            PlayerPreparation result;
            synchronized (preparations) {
                validate(request);
                var prepared = preparations.get(request.getOperationId());
                if (prepared != null) {
                    if (!prepared.getDelivery().equals(request)) {
                        throw new IllegalArgumentException(
                                "Operation reused with different delivery");
                    }
                } else {
                    if (preparations.size() >= 4096) {
                        throw new IllegalStateException(
                                "Process preparation history capacity reached");
                    }
                    var session =
                            manager.get(
                                    request.getSession().getId(), request.getSessionGeneration());
                    var reserved =
                            preparations.values().stream()
                                    .filter(
                                            candidate ->
                                                    candidate
                                                                    .getDelivery()
                                                                    .getSession()
                                                                    .equals(request.getSession())
                                                            && !candidate.isReleased())
                                    .count();
                    if (reserved >= session.getCommand().getCapacity())
                        throw new IllegalStateException("Session full");
                    prepared =
                            new PreparedDelivery(request, owners, now, session, manager.getTicks());
                    preparations.put(request.getOperationId(), prepared);
                }
                result = prepared.result(endpoint);
            }
            response.onNext(result);
            response.onCompleted();
        } catch (Exception ignored) {
            response.onError(
                    Status.FAILED_PRECONDITION
                            .withDescription("Delivery rejected")
                            .asRuntimeException());
        }
    }

    void flush() {
        synchronized (preparations) {
            preparations.values().forEach(PreparedDelivery::checkDeadline);
        }
    }

    List<DeliveryInventory> deliveries() {
        synchronized (preparations) {
            return preparations.values().stream().map(PreparedDelivery::inventory).toList();
        }
    }

    List<SessionInventory> sessions() {
        return manager.inventory().stream()
                .map(
                        session ->
                                session.toBuilder()
                                        .setPrepared(
                                                (int)
                                                        deliveries().stream()
                                                                .filter(
                                                                        delivery ->
                                                                                delivery.getDelivery()
                                                                                                .getSession()
                                                                                                .equals(
                                                                                                        session
                                                                                                                .getSession())
                                                                                        && delivery
                                                                                                        .getPhase()
                                                                                                == DeliveryPhase
                                                                                                        .DELIVERY_PHASE_PREPARED)
                                                                .count())
                                        .build())
                .toList();
    }

    @Override
    public void withdrawPlayer(
            PlayerWithdrawal request, StreamObserver<PlayerWithdrawal> response) {
        PreparedDelivery stream;
        synchronized (preparations) {
            stream = preparations.get(request.getOperationId());
        }
        if (stream == null
                || stream.getDelivery().getOwnerGeneration() != request.getOwnerGeneration()) {
            response.onError(Status.FAILED_PRECONDITION.asRuntimeException());
            return;
        }
        stream.close()
                .whenComplete(
                        (ignored, error) -> {
                            if (error != null) {
                                response.onError(
                                        Status.INTERNAL
                                                .withDescription("Withdrawal failed")
                                                .asRuntimeException());
                            } else {
                                response.onNext(request);
                                response.onCompleted();
                            }
                        });
    }

    void close() {
        MinecraftServer.getGlobalEventHandler().removeChild(events);
        synchronized (preparations) {
            preparations.values().forEach(PreparedDelivery::close);
        }
    }

    private void validate(PlayerDelivery delivery) {
        if (delivery.getSerializedSize() > 65_536)
            throw new IllegalArgumentException("Delivery exceeds size limit");
        if (!delivery.getDeployment().equals(deployment)
                || delivery.getProcessGeneration() != generation
                || !delivery.getRuntimeId().equals(runtimeId)) {
            throw new IllegalArgumentException("Stale process or deployment");
        }
        if (delivery.getProtocol() != MinecraftServer.PROTOCOL_VERSION) {
            throw new IllegalArgumentException("Incompatible destination protocol");
        }
        if (isBlank(delivery.getSession().getId())
                || delivery.getOperationId().isEmpty()
                || delivery.getOperationId().length() > 128) {
            throw new IllegalArgumentException("Unknown session or operation");
        }
        if (isBlank(delivery.getPlayer().getId())
                || delivery.getOwnerGeneration() <= 0
                || delivery.getMembershipGeneration() <= 0
                || isBlank(delivery.getProxyId())
                || isBlank(delivery.getConnectionId())) {
            throw new IllegalArgumentException("Invalid player ownership");
        }
        var identity = delivery.getIdentity();
        if (!USERNAME.matcher(identity.getUsername()).matches()
                || !UUID.fromString(identity.getUuid()).toString().equals(identity.getUuid())) {
            throw new IllegalArgumentException("Invalid player identity");
        }
    }

    private static boolean isBlank(String value) {
        return value.chars()
                .allMatch(
                        character ->
                                Character.isWhitespace(character)
                                        || Character.isSpaceChar(character));
    }
}
