package dev.chunkzero.runtime.control;

import org.jetbrains.annotations.ApiStatus;

import java.net.InetAddress;

/**
 * Whether an address is loopback or in 10/8, 172.16/12, 192.168/16, 100.64/10 or fc00::/7.
 * IPv4-mapped IPv6 addresses classify like their IPv4 address. Link-local addresses (169.254/16,
 * fe80::/10) are not private.
 */
@ApiStatus.Internal
public final class PrivateAddress {
    private PrivateAddress() {}

    public static boolean contains(InetAddress address) {
        var bytes = address.getAddress();
        if (bytes.length == 16 && mapped(bytes)) {
            return ipv4(bytes[12] & 0xff, bytes[13] & 0xff);
        }
        if (bytes.length == 4) {
            return ipv4(bytes[0] & 0xff, bytes[1] & 0xff);
        }
        return address.isLoopbackAddress() || (bytes[0] & 0xfe) == 0xfc;
    }

    private static boolean ipv4(int a, int b) {
        return a == 127
                || a == 10
                || (a == 172 && (b & 0xf0) == 16)
                || (a == 192 && b == 168)
                || (a == 100 && (b & 0xc0) == 64);
    }

    private static boolean mapped(byte[] bytes) {
        for (var i = 0; i < 10; i++) {
            if (bytes[i] != 0) {
                return false;
            }
        }
        return bytes[10] == (byte) 0xff && bytes[11] == (byte) 0xff;
    }
}
