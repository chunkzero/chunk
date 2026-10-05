package com.chunkzero.chunk.runtime.control;

import io.grpc.ManagedChannel;
import io.grpc.netty.shaded.io.grpc.netty.NettyChannelBuilder;

import org.jetbrains.annotations.ApiStatus;

import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.URI;
import java.util.concurrent.TimeUnit;

/** Channels to core's sync protocol, which carry messages of up to 16 MiB. */
@ApiStatus.Internal
public final class CoreChannel {
    public static final int MESSAGE_BYTES = 16 * 1024 * 1024;

    private CoreChannel() {}

    /**
     * A new channel to {@code endpoint}, which must be an HTTP address whose host is a private IP
     * literal. Hostnames are refused without being resolved.
     */
    public static ManagedChannel open(String endpoint) {
        var uri = URI.create(endpoint);
        var address = literal(uri.getHost());
        if (!"http".equals(uri.getScheme())
                || address == null
                || uri.getRawQuery() != null
                || uri.getFragment() != null
                || uri.getUserInfo() != null
                || (uri.getPath() != null && !uri.getPath().isEmpty() && !uri.getPath().equals("/"))
                || uri.getPort() < 1
                || uri.getPort() > 65535
                || !PrivateAddress.contains(address)) {
            throw new IllegalArgumentException(
                    "The core endpoint must be a private IP HTTP address");
        }
        return NettyChannelBuilder.forAddress(new InetSocketAddress(address, uri.getPort()))
                .overrideAuthority(uri.getRawAuthority())
                .usePlaintext()
                .maxInboundMessageSize(MESSAGE_BYTES)
                .keepAliveTime(30, TimeUnit.SECONDS)
                .keepAliveTimeout(10, TimeUnit.SECONDS)
                .build();
    }

    /**
     * The address {@code host} spells as an IPv4 or bracketed IPv6 literal, or null for anything
     * else.
     */
    private static InetAddress literal(String host) {
        if (host == null) {
            return null;
        }
        if (host.startsWith("[") && host.endsWith("]")) {
            host = host.substring(1, host.length() - 1);
        }
        try {
            return InetAddress.ofLiteral(host);
        } catch (IllegalArgumentException error) {
            return null;
        }
    }
}
