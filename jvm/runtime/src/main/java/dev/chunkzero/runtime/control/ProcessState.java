package dev.chunkzero.runtime.control;

import chunk.sync.v1.Jvm.JvmReport;

import com.google.protobuf.ByteString;

import org.jetbrains.annotations.ApiStatus;

import java.util.Map;

/** The sessions and deliveries an engine adapter holds, which it keeps in sync with core. */
@ApiStatus.Internal
public interface ProcessState {
    /** Every session and delivery the JVM holds, without health. */
    JvmReport inventory();

    /**
     * Makes the JVM's work match the latest snapshot of its {@code jvm/<host>} topic, keyed as in
     * the topic. Runs on the link's thread, never on a gRPC thread.
     */
    void apply(Map<String, ByteString> entries);
}
