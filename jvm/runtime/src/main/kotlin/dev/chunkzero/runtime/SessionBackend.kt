package dev.chunkzero.runtime

import chunk.v1.Common.DeploymentRef
import dev.chunkzero.backend.api.SessionId
import dev.chunkzero.backend.client.BackendSession
import dev.chunkzero.backend.client.SessionIdentity
import io.grpc.ManagedChannel
import io.grpc.netty.shaded.io.grpc.netty.NettyChannelBuilder
import java.net.InetAddress
import java.net.URI
import java.time.Duration
import java.util.Optional
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit

internal class SessionBackend(
    private val channel: ManagedChannel,
    private val credential: String,
    private val deployment: DeploymentRef,
) : AutoCloseable {
    private val scheduler = Executors.newSingleThreadScheduledExecutor()

    fun client(
        id: String,
        app: String,
    ) = BackendSession(
        channel,
        credential,
        deployment.environment,
        deployment.deployment,
        SessionIdentity(SessionId(id), app, Optional.empty()),
        scheduler,
        Duration.ofSeconds(5),
    )

    override fun close() {
        channel.shutdownNow().awaitTermination(3, TimeUnit.SECONDS)
        scheduler.shutdownNow()
    }

    companion object {
        fun fromEnvironment(
            deployment: DeploymentRef,
            environment: RuntimeEnvironment,
        ): SessionBackend? {
            val endpoint = environment.backendEndpoint ?: return null
            val credential = requireNotNull(environment.backendToken)
            val uri = URI(endpoint)
            require(uri.scheme == "http" && uri.port in 1..65535 && InetAddress.getByName(uri.host).isLoopbackAddress)
            return SessionBackend(
                NettyChannelBuilder.forAddress(uri.host, uri.port).usePlaintext().build(),
                credential,
                deployment,
            )
        }
    }
}
