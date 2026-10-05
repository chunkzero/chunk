package com.chunkzero.chunk.runtime.bootstrap;

import com.chunkzero.chunk.runtime.control.PrivateAddress;

import org.jetbrains.annotations.ApiStatus;
import org.jetbrains.annotations.Nullable;

import java.net.InetAddress;

/**
 * The launch configuration the platform supplies. {@code playerAddress} is where the JVM serves
 * players: {@code CHUNK_PLAYER_ADDRESS}, a loopback or private IP literal, or 127.0.0.1 when unset.
 */
@ApiStatus.Internal
public record RuntimeEnvironment(
        String processToken,
        String deployment,
        String coreEndpoint,
        String processId,
        long processGeneration,
        String machineProfile,
        String artifactDigest,
        String appId,
        InetAddress playerAddress) {
    public static RuntimeEnvironment load() {
        return new RuntimeEnvironment(
                required("CHUNK_PROCESS_TOKEN"),
                required("CHUNK_DEPLOYMENT"),
                endpoint(),
                required("CHUNK_PROCESS_ID"),
                Long.parseLong(required("CHUNK_PROCESS_GENERATION")),
                required("CHUNK_MACHINE_PROFILE"),
                required("CHUNK_ARTIFACT_DIGEST"),
                required("CHUNK_APP_ID"),
                playerAddress(System.getenv("CHUNK_PLAYER_ADDRESS")));
    }

    /** The loopback or private IP literal {@code value} spells, or 127.0.0.1 when unset. */
    static InetAddress playerAddress(@Nullable String value) {
        if (value == null || value.isBlank()) return InetAddress.ofLiteral("127.0.0.1");
        try {
            var address = InetAddress.ofLiteral(value);
            if (PrivateAddress.contains(address)) return address;
        } catch (IllegalArgumentException ignored) {
            /* Not an IP literal. */
        }
        throw new IllegalArgumentException(
                "CHUNK_PLAYER_ADDRESS must be a loopback or private IP literal");
    }

    /** {@code CHUNK_CORE_ENDPOINT}, or its older name {@code CHUNK_CONTROL_ENDPOINT}. */
    private static String endpoint() {
        var value = System.getenv("CHUNK_CORE_ENDPOINT");
        return value == null || value.isBlank() ? required("CHUNK_CONTROL_ENDPOINT") : value;
    }

    private static String required(String name) {
        var value = System.getenv(name);
        if (value == null || value.isBlank())
            throw new IllegalArgumentException(name + " is required");
        return value;
    }
}
