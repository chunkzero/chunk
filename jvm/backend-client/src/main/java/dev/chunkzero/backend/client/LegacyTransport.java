package dev.chunkzero.backend.client;

import chunk.sync.v1.CoreOuterClass.CallResponse;
import chunk.sync.v1.CoreOuterClass.Cursor;
import chunk.sync.v1.CoreOuterClass.Entry;
import chunk.sync.v1.CoreOuterClass.Error;
import chunk.sync.v1.CoreOuterClass.Position;
import chunk.sync.v1.CoreOuterClass.Update;
import chunk.v1.BackendGrpc;
import chunk.v1.BackendOuterClass.BackendMutation;
import chunk.v1.BackendOuterClass.BackendQuery;
import chunk.v1.BackendOuterClass.BackendResult;
import chunk.v1.BackendOuterClass.BackendUpdate;
import chunk.v1.BackendOuterClass.BackendWatchGroup;

import com.google.protobuf.ByteString;

import io.grpc.Channel;
import io.grpc.Metadata;
import io.grpc.stub.MetadataUtils;
import io.grpc.stub.StreamObserver;

import java.time.Duration;
import java.util.List;
import java.util.concurrent.TimeUnit;

/**
 * The {@code chunk.v1.Backend} service, bound to an environment and deployment by metadata. Each of
 * its group updates is a complete snapshot, and it never resumes.
 */
final class LegacyTransport implements Transport {
    private final BackendGrpc.BackendStub stub;

    LegacyTransport(Channel channel, String credential, String environment, String deployment) {
        if (environment == null
                || environment.isEmpty()
                || environment.length() > 128
                || deployment == null
                || deployment.isEmpty()
                || deployment.length() > 128)
            throw new IllegalArgumentException("Invalid environment or deployment");
        if (credential == null || credential.length() < 32)
            throw new IllegalArgumentException("Invalid backend credential");
        var metadata = new Metadata();
        metadata.put(
                Metadata.Key.of("authorization", Metadata.ASCII_STRING_MARSHALLER),
                "Bearer " + credential);
        metadata.put(
                Metadata.Key.of("x-chunk-environment", Metadata.ASCII_STRING_MARSHALLER),
                environment);
        metadata.put(
                Metadata.Key.of("x-chunk-deployment", Metadata.ASCII_STRING_MARSHALLER),
                deployment);
        stub =
                BackendGrpc.newStub(channel)
                        .withInterceptors(MetadataUtils.newAttachHeadersInterceptor(metadata));
    }

    @Override
    public void call(
            Invocation call,
            SessionIdentity caller,
            String operation,
            Duration deadline,
            StreamObserver<CallResponse> response) {
        var query = query(call, caller);
        var results =
                new StreamObserver<BackendResult>() {
                    public void onNext(BackendResult result) {
                        response.onNext(
                                CallResponse.newBuilder()
                                        .setPosition(position(result.getRevision()))
                                        .setResult(result.getResultJson())
                                        .build());
                    }

                    public void onError(Throwable error) {
                        response.onError(error);
                    }

                    public void onCompleted() {
                        response.onCompleted();
                    }
                };
        var timed = stub.withDeadlineAfter(deadline.toNanos(), TimeUnit.NANOSECONDS);
        if (operation.isEmpty()) {
            timed.query(query, results);
            return;
        }
        timed.mutate(
                BackendMutation.newBuilder()
                        .setFunction(query.getFunction())
                        .setArgumentsJson(query.getArgumentsJson())
                        .setCallerJson(query.getCallerJson())
                        .setOperationId(operation)
                        .build(),
                results);
    }

    @Override
    public void watch(
            List<Invocation> queries,
            SessionIdentity caller,
            Cursor after,
            StreamObserver<Update> updates) {
        var request =
                BackendWatchGroup.newBuilder()
                        .addAllQueries(queries.stream().map(query -> query(query, caller)).toList())
                        .build();
        stub.watchGroup(
                request,
                new StreamObserver<BackendUpdate>() {
                    public void onNext(BackendUpdate update) {
                        updates.onNext(snapshot(update));
                    }

                    public void onError(Throwable error) {
                        updates.onError(error);
                    }

                    public void onCompleted() {
                        updates.onCompleted();
                    }
                });
    }

    private static BackendQuery query(Invocation call, SessionIdentity caller) {
        return BackendQuery.newBuilder()
                .setFunction(call.function())
                .setArgumentsJson(call.arguments())
                .setCallerJson(ByteString.copyFromUtf8(caller.json().toString()))
                .build();
    }

    private static Update snapshot(BackendUpdate update) {
        var snapshot =
                Update.newBuilder().setPosition(position(update.getRevision())).setSnapshot(true);
        for (int index = 0; index < update.getResultsJsonCount(); index++) {
            var entry = Entry.newBuilder().setKey(Integer.toString(index));
            var error = index < update.getErrorsCount() ? update.getErrors(index) : "";
            if (error.isEmpty()) entry.setValue(update.getResultsJson(index));
            else
                entry.setError(
                        Error.newBuilder().setCode(Error.Code.CODE_APPLICATION).setMessage(error));
            snapshot.addUpserts(entry);
        }
        return snapshot.build();
    }

    private static Position position(long revision) {
        return Position.newBuilder().setRevision(revision).build();
    }
}
