package dev.chunkzero.runtime.bootstrap;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;

import java.net.InetAddress;

class RuntimeEnvironmentTest {
    @Test
    void playersAreServedOnLoopbackUnlessAPrivateAddressIsGiven() {
        assertEquals(InetAddress.ofLiteral("127.0.0.1"), RuntimeEnvironment.playerAddress(null));
        assertEquals(InetAddress.ofLiteral("127.0.0.1"), RuntimeEnvironment.playerAddress(""));
        assertEquals(
                InetAddress.ofLiteral("10.0.0.2"), RuntimeEnvironment.playerAddress("10.0.0.2"));
        assertEquals(InetAddress.ofLiteral("fdaa::2"), RuntimeEnvironment.playerAddress("fdaa::2"));
        for (var refused :
                new String[] {
                    "203.0.113.1", "2001:db8::1", "0.0.0.0", "169.254.169.254", "localhost"
                }) {
            assertThrows(
                    IllegalArgumentException.class,
                    () -> RuntimeEnvironment.playerAddress(refused),
                    refused);
        }
    }
}
