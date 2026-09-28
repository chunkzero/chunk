package dev.chunkzero.runtime.control;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;

import java.net.Inet6Address;
import java.net.InetAddress;
import java.util.Map;
import java.util.concurrent.TimeUnit;

class PrivateAddressTest {
    @Test
    void coversLoopbackAndPrivateRangesOnly() throws Exception {
        var cases =
                Map.ofEntries(
                        Map.entry("127.0.0.1", true),
                        Map.entry("10.255.255.255", true),
                        Map.entry("172.16.0.1", true),
                        Map.entry("172.31.255.255", true),
                        Map.entry("172.32.0.1", false),
                        Map.entry("192.168.1.1", true),
                        Map.entry("100.64.0.1", true),
                        Map.entry("100.127.255.255", true),
                        Map.entry("100.128.0.1", false),
                        Map.entry("169.254.169.254", false),
                        Map.entry("0.0.0.0", false),
                        Map.entry("224.0.0.1", false),
                        Map.entry("255.255.255.255", false),
                        Map.entry("8.8.8.8", false),
                        Map.entry("::1", true),
                        Map.entry("fc00::1", true),
                        Map.entry("fdaa::1", true),
                        Map.entry("fe80::1", false),
                        Map.entry("::", false),
                        Map.entry("ff02::1", false),
                        Map.entry("2001:db8::1", false));
        for (var entry : cases.entrySet()) {
            var address = InetAddress.getByName(entry.getKey());
            assertEquals(entry.getValue(), PrivateAddress.contains(address), entry.getKey());
            if (address.getAddress().length == 4) {
                assertEquals(entry.getValue(), PrivateAddress.contains(mapped(address)), "mapped " + entry.getKey());
            }
        }
    }

    @Test
    void coreChannelAcceptsPrivateEndpointsOnly() throws Exception {
        for (var endpoint : new String[] {"http://127.0.0.1:7070", "http://10.0.0.2:7070", "http://[fd00::1]:7070"}) {
            var channel = CoreChannel.open(endpoint);
            try {
                assertEquals(endpoint.substring("http://".length()), channel.authority());
            } finally {
                channel.shutdownNow().awaitTermination(5, TimeUnit.SECONDS);
            }
        }
        for (var endpoint :
                new String[] {"http://203.0.113.1:7070", "http://[2001:db8::1]:7070", "http://169.254.169.254:80"}) {
            assertThrows(IllegalArgumentException.class, () -> CoreChannel.open(endpoint), endpoint);
        }
    }

    /** The IPv4-mapped IPv6 form of {@code address}, which {@link InetAddress#getByName} would unwrap. */
    private static InetAddress mapped(InetAddress address) throws Exception {
        var bytes = new byte[16];
        bytes[10] = (byte) 0xff;
        bytes[11] = (byte) 0xff;
        System.arraycopy(address.getAddress(), 0, bytes, 12, 4);
        return Inet6Address.getByAddress(null, bytes, -1);
    }
}
