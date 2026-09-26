package dev.chunkzero.runtime;

import static org.junit.jupiter.api.Assertions.*;

import chunk.v1.BackendGrpc;
import chunk.v1.NodeControlGrpc;
import chunk.v1.Supervision.*;
import chunk.v1.SupervisorGrpc;

import com.google.protobuf.Empty;

import dev.chunkzero.runtime.bootstrap.RuntimeEnvironment;
import dev.chunkzero.runtime.control.ProcessState;

import io.grpc.*;
import io.grpc.stub.MetadataUtils;
import io.grpc.stub.StreamObserver;

import org.junit.jupiter.api.Test;

import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;

class ChunkProcessTest {
    private static final String TOKEN = "test-process-credential-with-32-bytes";

    @Test
    void connectsWithoutAManifestAndReadinessAndShutdownRemainAuthenticated() throws Exception {
        var registered = new AtomicReference<ProcessRegistration>();
        var reported = new CompletableFuture<ProcessReport>();
        var desired = new CompletableFuture<DesiredSessions>();
        var backend =
                ServerBuilder.forPort(0)
                        .addService(
                                new BackendGrpc.BackendImplBase() {
                                    @Override
                                    public void checkDeployment(
                                            Empty request, StreamObserver<Empty> response) {
                                        response.onNext(Empty.getDefaultInstance());
                                        response.onCompleted();
                                    }
                                })
                        .build()
                        .start();
        var supervisor =
                ServerBuilder.forPort(0)
                        .addService(
                                new SupervisorGrpc.SupervisorImplBase() {
                                    @Override
                                    public void registerProcess(
                                            ProcessRegistration request,
                                            StreamObserver<ProcessIdentity> response) {
                                        registered.set(request);
                                        response.onNext(request.getIdentity());
                                        response.onCompleted();
                                    }

                                    @Override
                                    public StreamObserver<ProcessReport> sync(
                                            StreamObserver<DesiredSessions> response) {
                                        return new StreamObserver<>() {
                                            @Override
                                            public void onNext(ProcessReport report) {
                                                reported.complete(report);
                                                response.onNext(
                                                        DesiredSessions.newBuilder()
                                                                .addCreate(
                                                                        SessionCommand
                                                                                .getDefaultInstance())
                                                                .build());
                                            }

                                            @Override
                                            public void onError(Throwable error) {}

                                            @Override
                                            public void onCompleted() {}
                                        };
                                    }
                                })
                        .build()
                        .start();
        var environment = environment(supervisor.getPort(), backend.getPort());
        try (var process = new ChunkProcess(environment)) {
            assertFalse(process.isReady());
            assertThrows(IllegalStateException.class, process::ready);
            process.bind(
                    List.of(),
                    "127.0.0.1:25565",
                    new ProcessState() {
                        @Override
                        public ProcessReport inventory() {
                            return ProcessReport.getDefaultInstance();
                        }

                        @Override
                        public void apply(DesiredSessions sessions) {
                            desired.complete(sessions);
                        }
                    });
            assertNull(registered.get());
            process.progress(2, 3);
            process.ready();
            assertTrue(process.isReady());
            assertEquals(process.identity(), registered.get().getIdentity());
            // The first flush after the stream opens reports everything, and control answers.
            process.flush();
            assertEquals(process.identity(), reported.get(3, TimeUnit.SECONDS).getIdentity());
            assertEquals(1, desired.get(3, TimeUnit.SECONDS).getCreateCount());
            var address = java.net.URI.create(registered.get().getControlEndpoint());
            var channel =
                    ManagedChannelBuilder.forAddress(address.getHost(), address.getPort())
                            .usePlaintext()
                            .build();
            try {
                var client =
                        NodeControlGrpc.newBlockingStub(channel)
                                .withDeadlineAfter(3, TimeUnit.SECONDS);
                var unauthorized =
                        assertThrows(
                                StatusRuntimeException.class,
                                () -> client.health(process.identity()));
                assertEquals(Status.Code.UNAUTHENTICATED, unauthorized.getStatus().getCode());
                var headers = new Metadata();
                headers.put(
                        Metadata.Key.of("authorization", Metadata.ASCII_STRING_MARSHALLER),
                        "Bearer " + TOKEN);
                var authenticated =
                        client.withInterceptors(MetadataUtils.newAttachHeadersInterceptor(headers));
                var health = authenticated.health(process.identity());
                assertTrue(health.getReady());
                assertEquals(1, health.getTickCount());
                assertEquals(2, health.getSessions());
                assertEquals(3, health.getPlayers());
                assertTrue(health.getHeapUsedBytes() > 0);
                var wrong = process.identity().toBuilder().setAppId("different").build();
                assertThrows(StatusRuntimeException.class, () -> authenticated.stopProcess(wrong));
                assertFalse(process.shutdownRequested().toCompletableFuture().isDone());
                authenticated.stopProcess(process.identity());
                process.shutdownRequested().toCompletableFuture().get(3, TimeUnit.SECONDS);
                assertFalse(process.isReady());
                process.ready();
                assertFalse(process.isReady());
                assertTrue(authenticated.health(process.identity()).getDraining());
            } finally {
                channel.shutdownNow().awaitTermination(3, TimeUnit.SECONDS);
            }
        } finally {
            supervisor.shutdownNow().awaitTermination(3, TimeUnit.SECONDS);
            backend.shutdownNow().awaitTermination(3, TimeUnit.SECONDS);
        }
    }

    private static RuntimeEnvironment environment(int supervisorPort, int backendPort) {
        return new RuntimeEnvironment(
                TOKEN,
                "test",
                "release",
                "http://127.0.0.1:" + supervisorPort,
                "node",
                "process",
                1,
                "local",
                "digest",
                "app",
                "http://127.0.0.1:" + backendPort,
                TOKEN);
    }
}
