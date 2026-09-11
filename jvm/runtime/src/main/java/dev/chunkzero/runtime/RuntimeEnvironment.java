package dev.chunkzero.runtime;

import org.jetbrains.annotations.Nullable;

record RuntimeEnvironment(
        String processToken,
        String environment,
        String deployment,
        @Nullable String supervisor,
        String runtimeId,
        String processId,
        long processGeneration,
        String machineProfile,
        String artifactDigest,
        boolean bootstrapSession,
        @Nullable String backendEndpoint,
        @Nullable String backendToken) {
    static RuntimeEnvironment load() {
        var token = System.getenv("CHUNK_PROCESS_TOKEN");
        if (token == null) throw new IllegalArgumentException("CHUNK_PROCESS_TOKEN is required");
        var generation = System.getenv("CHUNK_PROCESS_GENERATION");
        return new RuntimeEnvironment(
                token,
                required("CHUNK_ENVIRONMENT"),
                required("CHUNK_DEPLOYMENT"),
                System.getenv("CHUNK_SUPERVISOR"),
                valueOrDefault("CHUNK_RUNTIME_ID", "bridge"),
                valueOrDefault("CHUNK_PROCESS_ID", "bridge"),
                generation == null ? 1 : Long.parseLong(generation),
                valueOrDefault("CHUNK_MACHINE_PROFILE", "local"),
                valueOrDefault("CHUNK_ARTIFACT_DIGEST", "fixture"),
                !"".equals(System.getenv("CHUNK_BOOTSTRAP_SESSION")),
                System.getenv("CHUNK_BACKEND_ENDPOINT"),
                System.getenv("CHUNK_BACKEND_TOKEN"));
    }

    private static String required(String name) {
        var value = System.getenv(name);
        if (value == null || value.isBlank())
            throw new IllegalArgumentException(name + " is required");
        return value;
    }

    private static String valueOrDefault(String name, String fallback) {
        var value = System.getenv(name);
        return value == null ? fallback : value;
    }
}
