package com.chunkzero.chunk.backend.client;

import chunk.sync.v1.CoreOuterClass.CallResponse;
import chunk.sync.v1.CoreOuterClass.Cursor;
import chunk.sync.v1.CoreOuterClass.Update;

import com.google.protobuf.ByteString;

import io.grpc.stub.StreamObserver;

import java.time.Duration;
import java.util.List;

/**
 * Carries a session's calls and watches, answering in the sync protocol's messages. Cancelling the
 * gRPC context a call or watch starts in cancels it.
 */
interface Transport {
    /** Asks core for the operation ID of one action, which answers in a {@code PrepareResult}. */
    void prepare(Duration deadline, StreamObserver<CallResponse> response);

    /**
     * Starts one query, a mutation when {@code operation} is not empty, or an action when it is an
     * ID {@link #prepare} issued.
     */
    void call(
            Invocation call,
            SessionIdentity caller,
            String operation,
            Duration deadline,
            StreamObserver<CallResponse> response);

    /**
     * Opens one stream of the group's updates, keyed by each query's index, resuming after {@code
     * after} unless it is null.
     */
    void watch(
            List<Invocation> queries,
            SessionIdentity caller,
            Cursor after,
            StreamObserver<Update> updates);

    /** An app function and its encoded JSON arguments. */
    record Invocation(String function, ByteString arguments) {}
}
