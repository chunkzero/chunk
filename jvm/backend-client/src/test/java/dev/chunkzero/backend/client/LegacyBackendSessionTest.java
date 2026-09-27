package dev.chunkzero.backend.client;

import static org.junit.jupiter.api.Assertions.*;

import chunk.v1.BackendGrpc;
import chunk.v1.BackendOuterClass.*;

import com.google.protobuf.ByteString;

import dev.chunkzero.backend.api.*;

import io.grpc.ManagedChannelBuilder;
import io.grpc.Metadata;
import io.grpc.ServerBuilder;
import io.grpc.ServerCall;
import io.grpc.ServerCallHandler;
import io.grpc.ServerInterceptor;
import io.grpc.stub.StreamObserver;

import org.junit.jupiter.api.Test;

import tools.jackson.core.type.TypeReference;

import java.time.Duration;
import java.util.List;
import java.util.Optional;
import java.util.concurrent.Executors;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;

class LegacyBackendSessionTest {
    private static final String CREDENTIAL = "test-credential-with-at-least-32-bytes";
    private static final JsonType<Long> INTEGER =
            JsonType.of(new TypeReference<Long>() {}, BackendValues::checkInteger);
    private static final QueryRef<Long, Long> READ =
            new QueryRef<>("shared/read", INTEGER, INTEGER);

    @Test
    void publicConstructorSendsCallerJsonAndMetadataAndConvertsQueryErrors() throws Exception {
        var headers = new LinkedBlockingQueue<Metadata>();
        var queries = new LinkedBlockingQueue<BackendQuery>();
        var server =
                ServerBuilder.forPort(0)
                        .intercept(
                                new ServerInterceptor() {
                                    @Override
                                    public <Q, R> ServerCall.Listener<Q> interceptCall(
                                            ServerCall<Q, R> call,
                                            Metadata metadata,
                                            ServerCallHandler<Q, R> next) {
                                        headers.add(metadata);
                                        return next.startCall(call, metadata);
                                    }
                                })
                        .addService(
                                new BackendGrpc.BackendImplBase() {
                                    @Override
                                    public void query(
                                            BackendQuery request,
                                            StreamObserver<BackendResult> response) {
                                        queries.add(request);
                                        response.onNext(
                                                BackendResult.newBuilder()
                                                        .setRevision(1)
                                                        .setResultJson(ByteString.copyFromUtf8("3"))
                                                        .build());
                                        response.onCompleted();
                                    }

                                    @Override
                                    public void watchGroup(
                                            BackendWatchGroup request,
                                            StreamObserver<BackendUpdate> response) {
                                        response.onNext(
                                                BackendUpdate.newBuilder()
                                                        .setRevision(2)
                                                        .addResultsJson(
                                                                ByteString.copyFromUtf8("1"))
                                                        .addResultsJson(ByteString.EMPTY)
                                                        .addErrors("")
                                                        .addErrors("missing document")
                                                        .build());
                                    }
                                })
                        .build()
                        .start();
        var channel =
                ManagedChannelBuilder.forAddress("127.0.0.1", server.getPort())
                        .usePlaintext()
                        .build();
        var scheduler = Executors.newSingleThreadScheduledExecutor();
        try (var session =
                new BackendSession(
                        channel,
                        CREDENTIAL,
                        "local",
                        "immutable-build",
                        new SessionIdentity(
                                new SessionId("s1"), "duels", Optional.of(new PlayerId("trusted"))),
                        scheduler,
                        Duration.ofSeconds(5))) {
            assertEquals(3L, session.query(READ, 1L).get(2, TimeUnit.SECONDS));
            var query = queries.poll(2, TimeUnit.SECONDS);
            assertNotNull(query);
            assertEquals("shared/read", query.getFunction());
            var caller = BackendJson.mapper().readTree(query.getCallerJson().toStringUtf8());
            assertEquals("s1", caller.get("session").asString());
            assertEquals("duels", caller.get("app").asString());
            assertEquals("trusted", caller.get("player").asString());
            var metadata = headers.poll(2, TimeUnit.SECONDS);
            assertNotNull(metadata);
            assertEquals("Bearer " + CREDENTIAL, header(metadata, "authorization"));
            assertEquals("local", header(metadata, "x-chunk-environment"));
            assertEquals("immutable-build", header(metadata, "x-chunk-deployment"));

            var first = session.bind(READ, 1L);
            var second = session.bind(READ, 2L);
            var states = new LinkedBlockingQueue<GroupState>();
            try (var watch = session.watchGroup(List.of(first, second), states::add)) {
                assertNotNull(watch);
                GroupState fresh;
                do {
                    fresh = states.poll(2, TimeUnit.SECONDS);
                    assertNotNull(fresh);
                } while (fresh.stale());
                var snapshot = fresh.snapshot().orElseThrow();
                assertEquals(2, snapshot.revision());
                assertEquals(1L, snapshot.result(first).valueOrThrow());
                assertEquals(
                        new QueryResult.Failure<Long>("missing document"), snapshot.result(second));
            }
        } finally {
            channel.shutdownNow().awaitTermination(2, TimeUnit.SECONDS);
            server.shutdownNow().awaitTermination(2, TimeUnit.SECONDS);
            scheduler.shutdownNow();
        }
    }

    private static String header(Metadata metadata, String name) {
        return metadata.get(Metadata.Key.of(name, Metadata.ASCII_STRING_MARSHALLER));
    }
}
