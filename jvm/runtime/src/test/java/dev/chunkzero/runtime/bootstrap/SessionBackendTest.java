package dev.chunkzero.runtime.bootstrap;

import static org.junit.jupiter.api.Assertions.*;

import chunk.v1.BackendGrpc;
import chunk.v1.Common.DeploymentRef;

import com.google.protobuf.Empty;

import io.grpc.Context;
import io.grpc.Contexts;
import io.grpc.Metadata;
import io.grpc.ServerBuilder;
import io.grpc.ServerCall;
import io.grpc.ServerCallHandler;
import io.grpc.ServerInterceptor;
import io.grpc.Status;
import io.grpc.StatusRuntimeException;
import io.grpc.stub.StreamObserver;

import org.junit.jupiter.api.Test;

import java.util.concurrent.TimeUnit;

class SessionBackendTest {
    private static final String TOKEN = "test-credential-with-at-least-32-bytes";
    private static final Context.Key<String> DEPLOYMENT = Context.key("deployment");

    @Test
    void startupRequiresAnAvailableBackendForTheExactDeployment() throws Exception {
        var server =
                ServerBuilder.forPort(0)
                        .intercept(
                                new ServerInterceptor() {
                                    @Override
                                    public <Q, R> ServerCall.Listener<Q> interceptCall(
                                            ServerCall<Q, R> call,
                                            Metadata headers,
                                            ServerCallHandler<Q, R> next) {
                                        assertEquals(
                                                "Bearer " + TOKEN,
                                                headers.get(
                                                        Metadata.Key.of(
                                                                "authorization",
                                                                Metadata.ASCII_STRING_MARSHALLER)));
                                        assertEquals(
                                                "local",
                                                headers.get(
                                                        Metadata.Key.of(
                                                                "x-chunk-environment",
                                                                Metadata.ASCII_STRING_MARSHALLER)));
                                        var deployment =
                                                headers.get(
                                                        Metadata.Key.of(
                                                                "x-chunk-deployment",
                                                                Metadata.ASCII_STRING_MARSHALLER));
                                        return Contexts.interceptCall(
                                                Context.current().withValue(DEPLOYMENT, deployment),
                                                call,
                                                headers,
                                                next);
                                    }
                                })
                        .addService(
                                new BackendGrpc.BackendImplBase() {
                                    @Override
                                    public void checkDeployment(
                                            Empty request, StreamObserver<Empty> response) {
                                        if (!"build-a".equals(DEPLOYMENT.get())) {
                                            response.onError(
                                                    Status.NOT_FOUND
                                                            .withDescription(
                                                                    "deployment is unavailable")
                                                            .asRuntimeException());
                                            return;
                                        }
                                        response.onNext(Empty.getDefaultInstance());
                                        response.onCompleted();
                                    }
                                })
                        .build()
                        .start();
        try {
            var environment = environment("http://127.0.0.1:" + server.getPort(), false);
            try (var backend =
                    SessionBackend.fromEnvironment(deployment("build-a"), environment, true)) {
                assertNotNull(backend);
            }
            var failure =
                    assertThrows(
                            StatusRuntimeException.class,
                            () ->
                                    SessionBackend.fromEnvironment(
                                            deployment("build-b"), environment, true));
            assertEquals(Status.Code.NOT_FOUND, failure.getStatus().getCode());
            assertThrows(
                    IllegalArgumentException.class,
                    () ->
                            SessionBackend.fromEnvironment(
                                    deployment("build-a"), environment(null, false), true));
            assertThrows(
                    IllegalArgumentException.class,
                    () ->
                            SessionBackend.fromEnvironment(
                                    deployment("build-a"), environment(null, true), true));
            assertNull(
                    SessionBackend.fromEnvironment(
                            deployment("build-a"), environment(null, true), false));
        } finally {
            server.shutdownNow().awaitTermination(3, TimeUnit.SECONDS);
        }
    }

    private static DeploymentRef deployment(String id) {
        return DeploymentRef.newBuilder().setEnvironment("local").setDeployment(id).build();
    }

    private static RuntimeEnvironment environment(String endpoint, boolean fixture) {
        return new RuntimeEnvironment(
                TOKEN,
                "local",
                "build-a",
                null,
                "runtime",
                "process",
                1,
                "local",
                "artifact",
                fixture,
                endpoint,
                TOKEN);
    }
}
