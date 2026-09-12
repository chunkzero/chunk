package dev.chunkzero.runtime.bootstrap;

import org.jetbrains.annotations.ApiStatus;

@ApiStatus.Internal
public record RuntimeEnvironment(
        String processToken,
        String environment,
        String deployment,
        String controlEndpoint,
        String runtimeId,
        String processId,
        long processGeneration,
        String machineProfile,
        String artifactDigest,
        String appId,
        String backendEndpoint,
        String backendToken) {
    public static RuntimeEnvironment load() {
        return new RuntimeEnvironment(
                required("CHUNK_PROCESS_TOKEN"),
                required("CHUNK_ENVIRONMENT"),
                required("CHUNK_DEPLOYMENT"),
                required("CHUNK_CONTROL_ENDPOINT"),
                required("CHUNK_INSTANCE_ID"),
                required("CHUNK_PROCESS_ID"),
                Long.parseLong(required("CHUNK_PROCESS_GENERATION")),
                required("CHUNK_MACHINE_PROFILE"),
                required("CHUNK_ARTIFACT_DIGEST"),
                required("CHUNK_APP_ID"),
                required("CHUNK_BACKEND_ENDPOINT"),
                required("CHUNK_BACKEND_TOKEN"));
    }

    private static String required(String name) {
        var value = System.getenv(name);
        if (value == null || value.isBlank())
            throw new IllegalArgumentException(name + " is required");
        return value;
    }
}
