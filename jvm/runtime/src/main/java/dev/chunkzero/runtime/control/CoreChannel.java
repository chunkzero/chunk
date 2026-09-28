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

    /** A new channel to {@code endpoint}, which must be a private HTTP address. */
    public static ManagedChannel open(String endpoint) {
        var uri = URI.create(endpoint);
        var host = uri.getHost();
        if (host != null && host.startsWith("[") && host.endsWith("]")) {
            host = host.substring(1, host.length() - 1);
        }
        if (!"http".equals(uri.getScheme())
                || host == null
                || uri.getRawQuery() != null
                || uri.getFragment() != null
                || uri.getUserInfo() != null
                || (uri.getPath() != null && !uri.getPath().isEmpty() && !uri.getPath().equals("/"))
                || uri.getPort() < 1
                || uri.getPort() > 65535
                || !privateHost(host)) {
            throw new IllegalArgumentException("The core endpoint must be a private HTTP address");
        }
        return NettyChannelBuilder.forAddress(host, uri.getPort())
                .usePlaintext()
                .maxInboundMessageSize(MESSAGE_BYTES)
                .keepAliveTime(30, TimeUnit.SECONDS)
                .keepAliveTimeout(10, TimeUnit.SECONDS)
                .build();
    }

    private static boolean privateHost(String host) {
        try {
            return PrivateAddress.contains(InetAddress.getByName(host));
        } catch (UnknownHostException error) {
            return false;
        }
    }
}
