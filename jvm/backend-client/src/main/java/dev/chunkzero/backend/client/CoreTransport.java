package dev.chunkzero.backend.client;

import chunk.sync.v1.CoreGrpc;
import chunk.sync.v1.CoreOuterClass.CallRequest;
import chunk.sync.v1.CoreOuterClass.CallResponse;
import chunk.sync.v1.CoreOuterClass.Caller;
import chunk.sync.v1.CoreOuterClass.Cursor;
import chunk.sync.v1.CoreOuterClass.Error;
import chunk.sync.v1.CoreOuterClass.SubscribeRequest;
import chunk.sync.v1.CoreOuterClass.Update;

import com.google.protobuf.ByteString;

import dev.chunkzero.backend.api.BackendJson;
import dev.chunkzero.backend.api.PlayerId;

import io.grpc.Channel;
import io.grpc.Metadata;
import io.grpc.Status;
import io.grpc.stub.MetadataUtils;
import io.grpc.stub.StreamObserver;

import java.time.Duration;
import java.util.List;
import java.util.concurrent.TimeUnit;

/**
 * Core's sync protocol: app functions run as {@code Call}s and groups follow the {@code queries}
 * topic, in one deployment. Core derives the caller app code sees from the credential and the named
 * session and player.
 */
final class CoreTransport implements Transport {
    private static final int MESSAGE_BYTES = 16 * 1024 * 1024;
    private final CoreGrpc.CoreStub stub;
    private final String deployment;

    CoreTransport(Channel channel, String credential, String deployment) {
        if (credential == null || credential.length() < 32)
            throw new IllegalArgumentException("Invalid backend credential");
        if (deployment == null || deployment.isEmpty() || deployment.length() > 512)
            throw new IllegalArgumentException("Invalid deployment");
        var metadata = new Metadata();
        metadata.put(
                Metadata.Key.of("authorization", Metadata.ASCII_STRING_MARSHALLER),
                "Bearer " + credential);
        stub =
                CoreGrpc.newStub(channel)
                        .withInterceptors(MetadataUtils.newAttachHeadersInterceptor(metadata))
                        .withMaxInboundMessageSize(MESSAGE_BYTES);
        this.deployment = deployment;
    }

    @Override
    public void prepare(Duration deadline, StreamObserver<CallResponse> response) {
        var request = CallRequest.newBuilder().setMethod("chunk:prepare").build();
        stub.withDeadlineAfter(deadline.toNanos(), TimeUnit.NANOSECONDS).call(request, response);
    }

    @Override
    public void call(
            Invocation call,
            SessionIdentity caller,
            String operation,
            Duration deadline,
            StreamObserver<CallResponse> response) {
        var request =
                CallRequest.newBuilder()
                        .setOperationId(operation)
                        .setMethod(call.function())
                        .setArguments(call.arguments())
                        .setDeployment(deployment)
                        .setCaller(caller(caller))
                        .build();
        stub.withDeadlineAfter(deadline.toNanos(), TimeUnit.NANOSECONDS).call(request, response);
    }

    @Override
    public void watch(
            List<Invocation> queries,
            SessionIdentity caller,
            Cursor after,
            StreamObserver<Update> updates) {
        var group = BackendJson.mapper().createObjectNode();
        for (int index = 0; index < queries.size(); index++) {
            var query = queries.get(index);
            group.putObject(Integer.toString(index))
                    .put("function", query.function())
                    .set(
                            "arguments",
                            BackendJson.mapper().readTree(query.arguments().toStringUtf8()));
        }
        var request =
                SubscribeRequest.newBuilder()
                        .setTopic("queries")
                        .setArguments(ByteString.copyFromUtf8(group.toString()))
                        .setDeployment(deployment)
                        .setCaller(caller(caller));
        if (after != null) request.setAfter(after);
        stub.subscribe(request.build(), updates);
    }

    private static Caller caller(SessionIdentity identity) {
        return Caller.newBuilder()
                .setSession(identity.session().value())
                .setPlayer(identity.player().map(PlayerId::value).orElse(""))
                .build();
    }

    /** The gRPC status an in-band protocol error surfaces as. */
    static Status status(Error error) {
        var status =
                switch (error.getCode()) {
                    case CODE_UNAVAILABLE -> Status.UNAVAILABLE;
                    case CODE_OVERLOADED -> Status.RESOURCE_EXHAUSTED;
                    case CODE_INVALID, CODE_CONTRACT -> Status.INVALID_ARGUMENT;
                    case CODE_DENIED -> Status.PERMISSION_DENIED;
                    case CODE_OPERATION_MISMATCH -> Status.ALREADY_EXISTS;
                    case CODE_APPLICATION, CODE_STOPPED -> Status.FAILED_PRECONDITION;
                    default -> Status.UNKNOWN;
                };
        return status.withDescription(error.getCode() + ": " + error.getMessage());
    }
}
