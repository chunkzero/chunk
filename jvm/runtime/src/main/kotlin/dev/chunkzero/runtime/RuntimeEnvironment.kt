package dev.chunkzero.runtime

internal data class RuntimeEnvironment(
    val processToken: String,
    val environment: String,
    val deployment: String,
    val supervisor: String?,
    val runtimeId: String,
    val processId: String,
    val processGeneration: Long,
    val machineProfile: String,
    val artifactDigest: String,
) {
    companion object {
        fun load() =
            RuntimeEnvironment(
                processToken =
                    requireNotNull(
                        System.getenv("CHUNK_PROCESS_TOKEN"),
                    ) { "CHUNK_PROCESS_TOKEN is required" },
                environment = System.getenv("CHUNK_ENVIRONMENT") ?: "local",
                deployment = System.getenv("CHUNK_DEPLOYMENT") ?: "local",
                supervisor = System.getenv("CHUNK_SUPERVISOR"),
                runtimeId = System.getenv("CHUNK_RUNTIME_ID") ?: "bridge",
                processId = System.getenv("CHUNK_PROCESS_ID") ?: "bridge",
                processGeneration = System.getenv("CHUNK_PROCESS_GENERATION")?.toLong() ?: 1,
                machineProfile = System.getenv("CHUNK_MACHINE_PROFILE") ?: "local",
                artifactDigest = System.getenv("CHUNK_ARTIFACT_DIGEST") ?: "fixture",
            )
    }
}
