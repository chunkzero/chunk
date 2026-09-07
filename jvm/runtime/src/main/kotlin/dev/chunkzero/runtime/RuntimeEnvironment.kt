package dev.chunkzero.runtime

internal data class RuntimeEnvironment(
    val processToken: String,
    val environment: String,
    val deployment: String,
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
            )
    }
}
