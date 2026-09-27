package dev.chunkzero.runtime;

import static org.junit.jupiter.api.Assertions.*;

import chunk.sync.v1.CoreGrpc;
import chunk.sync.v1.CoreOuterClass.CallRequest;
import chunk.sync.v1.CoreOuterClass.CallResponse;
import chunk.sync.v1.CoreOuterClass.Entry;
import chunk.sync.v1.CoreOuterClass.Error;
import chunk.sync.v1.CoreOuterClass.SubscribeRequest;
import chunk.sync.v1.CoreOuterClass.Update;
import chunk.sync.v1.Jvm.JvmRegistered;
import chunk.sync.v1.Jvm.JvmRegistration;
import chunk.sync.v1.Jvm.JvmReport;
import chunk.sync.v1.Jvm.JvmSession;
import chunk.sync.v1.Jvm.JvmSessionPhase;
import chunk.sync.v1.Jvm.JvmSessionStatus;
import chunk.sync.v1.Jvm.JvmStop;

import com.google.protobuf.ByteString;

import dev.chunkzero.runtime.bootstrap.RuntimeEnvironment;
import dev.chunkzero.runtime.control.ProcessState;

import io.grpc.Metadata;
import io.grpc.Server;
import io.grpc.ServerBuilder;
import io.grpc.ServerCall;
import io.grpc.ServerCallHandler;
import io.grpc.ServerInterceptor;
import io.grpc.stub.StreamObserver;

import org.junit.jupiter.api.Test;

import java.util.Map;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;

class ChunkProcessTest {
    private static final String TOKEN = "test-process-credential-with-32-bytes";

    @Test
    void registersThenFollowsItsTopicAndReportsUntilCoreStopsIt() throws Exception {
        var core = new FakeCore();
        var server = core.start();
        var applied = new LinkedBlockingQueue<Map<String, ByteString>>();
        var session =
                JvmSessionStatus.newBuilder()
                        .setId("a")
                        .setSessionType("app/lobby")
                        .setCapacity(4)
                        .setPhase(JvmSessionPhase.JVM_SESSION_PHASE_READY)
                        .build();
        var inventory = new AtomicReference<>(JvmReport.newBuilder().addSessions(session).build());
        var state =
                new ProcessState() {
                    @Override
                    public JvmReport inventory() {
                        return inventory.get();
                    }

                    @Override
                    public void apply(Map<String, ByteString> entries) {
                        applied.add(entries);
                    }
                };
        try (var process = new ChunkProcess(environment(server.getPort()))) {
            assertFalse(process.isReady());
            assertThrows(IllegalStateException.class, process::ready);
            process.bind("127.0.0.1:25566", 775, state);
            process.progress(2, 3);
            process.ready();
            assertTrue(process.isReady());

            var registration =
                    JvmRegistration.parseFrom(core.call("chunk:register").getArguments());
            assertEquals("release", registration.getDeployment());
            assertEquals("127.0.0.1:25566", registration.getPlayerEndpoint());
            assertEquals(775, registration.getProtocol());
            assertEquals("jvm/host", core.subscriptions.poll(5, TimeUnit.SECONDS).getTopic());
            var first = core.call("chunk:report");
            assertEquals("stream-1", first.getStream());
            var complete = JvmReport.parseFrom(first.getArguments());
            assertTrue(complete.getComplete());
            assertEquals(session, complete.getSessions(0));
            assertTrue(complete.getHealth().getReady());
            assertEquals(1, complete.getHealth().getTickCount());
            assertEquals(3, complete.getHealth().getPlayers());
            assertTrue(applied.poll(5, TimeUnit.SECONDS).containsKey("session/a"));

            // Later reports carry only what changed, and health at least every few seconds.
            var ending = session.toBuilder().setPhase(JvmSessionPhase.JVM_SESSION_PHASE_ENDING);
            inventory.set(JvmReport.newBuilder().addSessions(ending).build());
            process.flush();
            var changed = JvmReport.parseFrom(core.call("chunk:report").getArguments());
            assertFalse(changed.getComplete());
            assertEquals(ending.build(), changed.getSessions(0));
            var pushed = JvmReport.parseFrom(core.call("chunk:report").getArguments());
            assertEquals(0, pushed.getSessionsCount());
            assertTrue(pushed.hasHealth());

            // A superseded stream registers and subscribes again, and reports everything first.
            core.end(Error.Code.CODE_STOPPED);
            core.call("chunk:register");
            core.subscriptions.poll(5, TimeUnit.SECONDS);
            var resumed = core.call("chunk:report");
            assertEquals("stream-2", resumed.getStream());
            assertTrue(JvmReport.parseFrom(resumed.getArguments()).getComplete());

            assertFalse(process.shutdownRequested().toCompletableFuture().isDone());
            core.send("stop", JvmStop.getDefaultInstance().toByteString());
            process.shutdownRequested().toCompletableFuture().get(5, TimeUnit.SECONDS);
            assertFalse(process.isReady());
        } finally {
            server.shutdownNow().awaitTermination(3, TimeUnit.SECONDS);
        }
    }

    private static RuntimeEnvironment environment(int port) {
        return new RuntimeEnvironment(
                TOKEN,
                "release",
                "http://127.0.0.1:" + port,
                "process",
                1,
                "local",
                "digest",
                "app");
    }

    /** Registers the JVM as {@code host} and serves its topic, numbering each stream. */
    private static final class FakeCore extends CoreGrpc.CoreImplBase {
        final BlockingQueue<CallRequest> calls = new LinkedBlockingQueue<>();
        final BlockingQueue<SubscribeRequest> subscriptions = new LinkedBlockingQueue<>();
        private StreamObserver<Update> topic;
        private String stream = "";
        private int streams;

        Server start() throws Exception {
            return ServerBuilder.forPort(0)
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
                                    return next.startCall(call, headers);
                                }
                            })
                    .addService(this)
                    .build()
                    .start();
        }

        /** The next call, which must be of {@code method}. */
        CallRequest call(String method) throws InterruptedException {
            var request = calls.poll(5, TimeUnit.SECONDS);
            assertNotNull(request, "no " + method);
            assertEquals(method, request.getMethod());
            return request;
        }

        @Override
        public synchronized void call(CallRequest request, StreamObserver<CallResponse> response) {
            calls.add(request);
            var result = CallResponse.newBuilder();
            if (request.getMethod().equals("chunk:register"))
                result.setResult(JvmRegistered.newBuilder().setHost("host").build().toByteString());
            else if (!request.getStream().equals(stream))
                result.setError(Error.newBuilder().setCode(Error.Code.CODE_STOPPED));
            else result.setResult(ByteString.EMPTY);
            response.onNext(result.build());
            response.onCompleted();
        }

        @Override
        public synchronized void subscribe(
                SubscribeRequest request, StreamObserver<Update> response) {
            subscriptions.add(request);
            topic = response;
            stream = "stream-" + ++streams;
            response.onNext(
                    Update.newBuilder()
                            .setStream(stream)
                            .setSnapshot(true)
                            .addUpserts(
                                    Entry.newBuilder()
                                            .setKey("session/a")
                                            .setValue(
                                                    JvmSession.newBuilder()
                                                            .setSessionType("app/lobby")
                                                            .setCapacity(4)
                                                            .build()
                                                            .toByteString()))
                            .build());
        }

        synchronized void send(String key, ByteString value) {
            topic.onNext(
                    Update.newBuilder()
                            .setSnapshot(true)
                            .addUpserts(Entry.newBuilder().setKey(key).setValue(value))
                            .build());
        }

        synchronized void end(Error.Code code) {
            topic.onNext(Update.newBuilder().setError(Error.newBuilder().setCode(code)).build());
            topic.onCompleted();
            stream = "";
        }
    }
}
