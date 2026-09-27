package dev.chunkzero.runtime.bootstrap;

import org.jetbrains.annotations.ApiStatus;

@ApiStatus.Internal
public record RuntimeEnvironment(
        String processToken,
        String deployment,
        String coreEndpoint,
        String processId,
        long processGeneration,
        String machineProfile,
        String artifactDigest,
        String appId) {
    public static RuntimeEnvironment load() {
        return new RuntimeEnvironment(
                required("CHUNK_PROCESS_TOKEN"),
                required("CHUNK_DEPLOYMENT"),
                endpoint(),
                required("CHUNK_PROCESS_ID"),
                Long.parseLong(required("CHUNK_PROCESS_GENERATION")),
                required("CHUNK_MACHINE_PROFILE"),
                required("CHUNK_ARTIFACT_DIGEST"),
                required("CHUNK_APP_ID"));
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
