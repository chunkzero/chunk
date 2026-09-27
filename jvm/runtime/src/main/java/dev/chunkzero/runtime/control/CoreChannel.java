package dev.chunkzero.runtime.control;

import io.grpc.ManagedChannel;
import io.grpc.netty.shaded.io.grpc.netty.NettyChannelBuilder;

import org.jetbrains.annotations.ApiStatus;

import java.net.InetAddress;
import java.net.URI;
import java.net.UnknownHostException;
import java.util.concurrent.TimeUnit;

/** Channels to core's sync protocol, which carry messages of up to 16 MiB. */
@ApiStatus.Internal
public final class CoreChannel {
    public static final int MESSAGE_BYTES = 16 * 1024 * 1024;

    private CoreChannel() {}

    /** A new channel to {@code endpoint}, which must be a loopback HTTP address. */
    public static ManagedChannel open(String endpoint) {
        var uri = URI.create(endpoint);
        if (!"http".equals(uri.getScheme())
                || uri.getHost() == null
                || uri.getRawQuery() != null
                || uri.getFragment() != null
                || uri.getUserInfo() != null
                || (uri.getPath() != null && !uri.getPath().isEmpty() && !uri.getPath().equals("/"))
                || uri.getPort() < 1
                || uri.getPort() > 65535
                || !loopback(uri.getHost())) {
            throw new IllegalArgumentException("The core endpoint must be a loopback HTTP address");
        }
        return NettyChannelBuilder.forAddress(uri.getHost(), uri.getPort())
                .usePlaintext()
                .maxInboundMessageSize(MESSAGE_BYTES)
                .keepAliveTime(30, TimeUnit.SECONDS)
                .keepAliveTimeout(10, TimeUnit.SECONDS)
                .build();
    }

    private static boolean loopback(String host) {
        try {
            return InetAddress.getByName(host).isLoopbackAddress();
        } catch (UnknownHostException error) {
            return false;
        }
    }
}
