package com.chunkzero.chunk.runtime;

import static org.junit.jupiter.api.Assertions.*;

import com.chunkzero.chunk.backend.client.BackendSession;

import org.junit.jupiter.api.Test;

import java.util.HashMap;
import java.util.List;
import java.util.Map;

class ComponentRegistryTest {
    @Test
    void sessionComponentsReceiveOnlyMarkedSuppliedTypes() {
        var binding =
                new ComponentBinding<>(
                        Greeting.class,
                        Component.Scope.SESSION,
                        List.of(Handle.class),
                        deps -> new Greeting((Handle) deps[0]));
        try (var registry = new ComponentRegistry(List.of(binding))) {
            var handle = new Handle();
            var components = registry.session(Map.of(Handle.class, handle));
            assertSame(handle, components.get(Greeting.class).handle());
            assertThrows(
                    IllegalArgumentException.class,
                    () -> registry.session(Map.of(String.class, "unmarked")));
            assertThrows(
                    IllegalArgumentException.class,
                    () -> registry.session(Map.of(Handle.class, "not a handle")));
            var missingBackend = new HashMap<Class<?>, Object>();
            missingBackend.put(BackendSession.class, null);
            assertThrows(IllegalArgumentException.class, () -> registry.session(missingBackend));
            assertThrows(
                    IllegalStateException.class,
                    () -> registry.session(Map.of()).get(Greeting.class));
        }
        assertThrows(
                IllegalArgumentException.class,
                () ->
                        new ComponentRegistry(
                                List.of(
                                        new ComponentBinding<>(
                                                Handle.class,
                                                Component.Scope.SESSION,
                                                List.of(),
                                                deps -> new Handle()))));
    }

    @Test
    void factoryReturningSuppliedInstanceIsRejectedWithoutClosingIt() {
        var binding =
                new ComponentBinding<>(
                        Resource.class,
                        Component.Scope.SESSION,
                        List.of(Channel.class),
                        deps -> (Resource) deps[0]);
        var socket = new Socket();
        try (var registry = new ComponentRegistry(List.of(binding))) {
            try (var components = registry.session(Map.of(Channel.class, socket))) {
                assertThrows(IllegalStateException.class, () -> components.get(Resource.class));
            }
        }
        assertFalse(socket.closed);
    }

    @Component.Supplied
    private static final class Handle {}

    @Component.Supplied
    private interface Channel {}

    private interface Resource extends AutoCloseable {
        @Override
        void close();
    }

    private static final class Socket implements Channel, Resource {
        boolean closed;

        @Override
        public void close() {
            closed = true;
        }
    }

    private record Greeting(Handle handle) {}
}
