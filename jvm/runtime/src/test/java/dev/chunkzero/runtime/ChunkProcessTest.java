package dev.chunkzero.runtime;

import static org.junit.jupiter.api.Assertions.*;

import chunk.v1.BackendGrpc;
import chunk.v1.NodeControlGrpc;
import chunk.v1.Supervision.*;
import chunk.v1.SupervisorGrpc;

import com.google.protobuf.Empty;

import dev.chunkzero.runtime.bootstrap.RuntimeEnvironment;

import io.grpc.*;
import io.grpc.stub.MetadataUtils;
import io.grpc.stub.StreamObserver;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;

class ChunkProcessTest {
    @TempDir Path directory;
    private static final String TOKEN = "test-process-credential-with-32-bytes";

    @Test
    void missingAndDuplicateManifestsAreRejectedBeforeConnecting() throws Exception {
        var environment = environment(1, 1);
        try (var loader = new URLClassLoader(new URL[0], getClass().getClassLoader())) {
            var error =
                    assertThrows(
                            IllegalArgumentException.class,
                            () -> new ChunkProcess(environment, loader));
            assertTrue(error.getMessage().contains("exactly one"), error.toString());
        }
        var urls = new URL[2];
        for (var index = 0; index < urls.length; index++) {
            var root = directory.resolve("app-" + index);
            var metadata = root.resolve("META-INF/chunk/app.json");
            Files.createDirectories(metadata.getParent());
            Files.writeString(metadata, "{}");
            urls[index] = root.toUri().toURL();
        }
        try (var loader = new URLClassLoader(urls, getClass().getClassLoader())) {
            var error =
                    assertThrows(
                            IllegalArgumentException.class,
                            () -> new ChunkProcess(environment, loader));
            assertTrue(error.getMessage().contains("exactly one"), error.toString());
        }
    }

    @Test
    void readinessIsExplicitAndShutdownIsAuthenticatedAndIrreversible() throws Exception {
        var registered = new AtomicReference<ProcessRegistration>();
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
                                })
                        .build()
                        .start();
        var metadata = directory.resolve("META-INF/chunk/app.json");
        Files.createDirectories(metadata.getParent());
        Files.writeString(
                metadata,
                """
                {"version":2,"id":"app","main_class":"test.Main","sessions":{
                    "default":{"provider":"test.Factory","machine_profile":"local","capacity":16}}}
                """);
        var environment = environment(supervisor.getPort(), backend.getPort());
        try (var loader =
                        new URLClassLoader(
                                new URL[] {directory.toUri().toURL()},
                                getClass().getClassLoader());
                var process = new ChunkProcess(environment, loader)) {
            assertFalse(process.isReady());
            assertThrows(IllegalStateException.class, process::ready);
            process.bind(List.of(), "127.0.0.1:25565");
            assertNull(registered.get());
            process.progress(2, 3);
            process.ready();
            assertTrue(process.isReady());
            assertEquals(process.identity(), registered.get().getIdentity());
            assertEquals(64, registered.get().getManifestDigest().length());
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
