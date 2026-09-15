package dev.chunkzero.runtime;

import static dev.chunkzero.runtime.Component.Scope.PROCESS;
import static dev.chunkzero.runtime.Component.Scope.SESSION;

import static org.junit.jupiter.api.Assertions.*;

import dev.chunkzero.backend.client.BackendSession;
import dev.chunkzero.runtime.minestom.internal.ComponentRegistry;

import net.minestom.server.MinecraftServer;

import org.junit.jupiter.api.Test;

import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;

final class ComponentRegistryTest {
    @Test
    void sessionsAreIndependentAndProcessDependenciesOutliveTheirConsumers() throws Exception {
        var events = new ArrayList<String>();
        var created = new AtomicInteger();
        var bindings =
                List.<ComponentBinding<?>>of(
                        binding(
                                Shared.class,
                                PROCESS,
                                List.of(),
                                deps -> {
                                    created.incrementAndGet();
                                    return new Shared(() -> events.add("process"));
                                }),
                        binding(
                                Scoped.class,
                                SESSION,
                                List.of(Shared.class, SessionScope.class),
                                deps -> {
                                    var scope = (SessionScope) deps[1];
                                    return new Scoped(
                                            (Shared) deps[0],
                                            () -> events.add("dependency-" + scope.getId()));
                                }),
                        binding(
                                Root.class,
                                SESSION,
                                List.of(Scoped.class, SessionScope.class),
                                deps -> {
                                    var scope = (SessionScope) deps[1];
                                    return new Root(
                                            (Scoped) deps[0],
                                            () -> {
                                                events.add("root-" + scope.getId());
                                                if (scope.getId().equals("a"))
                                                    throw new IllegalStateException("Close failed");
                                            });
                                }));
        try (var world = new World(bindings)) {
            var first = world.scope("a");
            var second = world.scope("b");
            var a = first.component(Root.class);
            var b = second.component(Root.class);
            assertSame(a, first.component(Root.class));
            assertNotSame(a, b);
            assertNotSame(a.dependency, b.dependency);
            assertSame(a.dependency.shared, b.dependency.shared);
            assertEquals(1, created.get());
            var failed = assertThrows(IllegalStateException.class, first::dispose);
            assertEquals(1, failed.getSuppressed().length);
            assertEquals(List.of("root-a", "dependency-a"), events);
            assertSame(b, second.component(Root.class));
            assertThrows(IllegalStateException.class, () -> first.component(Root.class));
            second.dispose();
            world.registry.close();
            assertEquals(
                    List.of("root-a", "dependency-a", "root-b", "dependency-b", "process"), events);
            first.dispose();
            world.registry.close();
            assertEquals(5, events.size());
        }
    }

    @Test
    void failedConstructionRollsBackOnlyNewDependenciesEvenWhenCleanupThrows() throws Exception {
        for (var warm : List.of(false, true)) {
            var events = new ArrayList<String>();
            var created = new AtomicInteger();
            var firstAttempt = new AtomicBoolean(true);
            var failure = new IllegalStateException("Factory failed");
            var bindings =
                    List.<ComponentBinding<?>>of(
                            binding(
                                    Shared.class,
                                    PROCESS,
                                    List.of(),
                                    deps -> {
                                        var generation = created.incrementAndGet();
                                        return new Shared(
                                                () -> events.add("process-" + generation));
                                    }),
                            binding(
                                    Scoped.class,
                                    SESSION,
                                    List.of(Shared.class),
                                    deps -> {
                                        var failClose = firstAttempt.get();
                                        return new Scoped(
                                                (Shared) deps[0],
                                                () -> {
                                                    events.add("dependency");
                                                    if (failClose)
                                                        throw new AssertionError("Disposal failed");
                                                });
                                    }),
                            binding(
                                    Root.class,
                                    SESSION,
                                    List.of(Scoped.class),
                                    deps -> {
                                        if (firstAttempt.getAndSet(false)) throw failure;
                                        return new Root((Scoped) deps[0], () -> events.add("root"));
                                    }));
            try (var world = new World(bindings)) {
                var scope = world.scope("failed");
                var cached = warm ? world.scope("independent").component(Shared.class) : null;
                assertSame(
                        failure,
                        assertThrows(
                                IllegalStateException.class, () -> scope.component(Root.class)));
                assertInstanceOf(AssertionError.class, failure.getSuppressed()[0]);
                assertEquals(
                        warm ? List.of("dependency") : List.of("dependency", "process-1"), events);
                var retry = scope.component(Root.class);
                assertEquals(warm ? 1 : 2, created.get());
                if (warm) assertSame(cached, retry.dependency.shared);
                assertSame(retry, scope.component(Root.class));
                scope.dispose();
                world.registry.close();
                assertEquals(
                        warm ? 1 : 2,
                        events.stream().filter(value -> value.startsWith("process")).count());
            }
        }
    }

    @Test
    void graphValidationRejectsScopeCaptureCyclesAndMissingExactTypesBeforeAnyFactoryRuns() {
        var created = new AtomicInteger();
        var shared =
                binding(
                        Shared.class,
                        PROCESS,
                        List.of(),
                        deps -> {
                            created.incrementAndGet();
                            return new Shared(() -> {});
                        });
        assertThrows(
                IllegalArgumentException.class,
                () -> new ComponentRegistry(List.of(shared, shared)));
        assertThrows(
                IllegalArgumentException.class,
                () ->
                        new ComponentRegistry(
                                List.of(
                                        binding(
                                                Root.class,
                                                SESSION,
                                                List.of(Scoped.class),
                                                deps -> null))));
        assertThrows(
                IllegalArgumentException.class,
                () ->
                        new ComponentRegistry(
                                List.of(
                                        binding(
                                                Shared.class,
                                                PROCESS,
                                                List.of(Scoped.class),
                                                deps -> null),
                                        binding(Scoped.class, SESSION, List.of(), deps -> null))));
        assertThrows(
                IllegalArgumentException.class,
                () ->
                        new ComponentRegistry(
                                List.of(
                                        binding(
                                                Shared.class,
                                                PROCESS,
                                                List.of(SessionScope.class),
                                                deps -> null))));
        assertThrows(
                IllegalArgumentException.class,
                () ->
                        new ComponentRegistry(
                                List.of(
                                        binding(
                                                Shared.class,
                                                PROCESS,
                                                List.of(BackendSession.class),
                                                deps -> null))));
        assertThrows(
                IllegalArgumentException.class,
                () ->
                        new ComponentRegistry(
                                List.of(
                                        binding(
                                                Shared.class,
                                                SESSION,
                                                List.of(Scoped.class),
                                                deps -> null),
                                        binding(
                                                Scoped.class,
                                                SESSION,
                                                List.of(Shared.class),
                                                deps -> null))));
        assertThrows(
                IllegalArgumentException.class,
                () ->
                        new ComponentRegistry(
                                List.of(
                                        binding(
                                                SessionScope.class,
                                                SESSION,
                                                List.of(),
                                                deps -> null))));
        assertEquals(0, created.get());
    }

    @Test
    void factoriesCannotResolveUndeclaredDependenciesOrCloseTheRegistryReentrantly()
            throws Exception {
        var closed = new AtomicInteger();
        var reentrant = new AtomicBoolean(true);
        var bindings =
                List.<ComponentBinding<?>>of(
                        binding(
                                Scoped.class,
                                SESSION,
                                List.of(),
                                deps -> new Scoped(null, closed::incrementAndGet)),
                        binding(
                                Root.class,
                                SESSION,
                                List.of(Scoped.class, SessionScope.class),
                                deps -> {
                                    if (reentrant.getAndSet(false))
                                        ((SessionScope) deps[1]).component(Scoped.class);
                                    return new Root((Scoped) deps[0], () -> {});
                                }));
        try (var world = new World(bindings)) {
            var scope = world.scope("session");
            var failure =
                    assertThrows(IllegalStateException.class, () -> scope.component(Root.class));
            assertTrue(failure.getMessage().startsWith("Reentrant component access"));
            assertEquals(1, closed.get());
            assertNotNull(scope.component(Root.class));
            scope.dispose();
            assertEquals(2, closed.get());
        }
        var registry = new ComponentRegistry[1];
        registry[0] =
                new ComponentRegistry(
                        List.of(
                                binding(
                                        Shared.class,
                                        PROCESS,
                                        List.of(),
                                        deps -> {
                                            registry[0].close();
                                            return new Shared(() -> {});
                                        })));
        try (var world = new World(registry[0])) {
            assertThrows(
                    IllegalStateException.class,
                    () -> world.scope("session").component(Shared.class));
            assertDoesNotThrow(registry[0]::close);
        }
    }

    @Test
    void returnedAliasesCannotTakeOwnershipOfCachedOrBorrowedResources() throws Exception {
        var closed = new AtomicInteger();
        var bindings =
                List.<ComponentBinding<?>>of(
                        binding(
                                Scoped.class,
                                SESSION,
                                List.of(),
                                deps -> new Scoped(null, closed::incrementAndGet)),
                        binding(
                                AutoCloseable.class,
                                SESSION,
                                List.of(Scoped.class),
                                deps -> (Scoped) deps[0]),
                        binding(
                                Object.class,
                                SESSION,
                                List.of(SessionScope.class),
                                deps -> deps[0]));
        try (var world = new World(bindings)) {
            var scope = world.scope("session");
            var existing = scope.component(Scoped.class);
            assertThrows(IllegalStateException.class, () -> scope.component(AutoCloseable.class));
            assertEquals(0, closed.get());
            assertSame(existing, scope.component(Scoped.class));
            assertThrows(IllegalStateException.class, () -> scope.component(Object.class));
            assertSame(scope, scope.component(SessionScope.class));
            scope.dispose();
            assertEquals(1, closed.get());
        }
    }

    @Test
    void processShutdownAttemptsOtherSessionsAndProcessCleanupAfterSessionFailure() {
        var events = new ArrayList<String>();
        var bindings =
                List.<ComponentBinding<?>>of(
                        binding(
                                Shared.class,
                                PROCESS,
                                List.of(),
                                deps -> new Shared(() -> events.add("process"))),
                        binding(
                                Scoped.class,
                                SESSION,
                                List.of(Shared.class, SessionScope.class),
                                deps -> {
                                    var scope = (SessionScope) deps[1];
                                    return new Scoped(
                                            (Shared) deps[0],
                                            () -> {
                                                events.add(scope.getId());
                                                if (scope.getId().equals("b"))
                                                    throw new AssertionError("Close failed");
                                            });
                                }));
        try (var world = new World(bindings)) {
            var first = world.scope("a");
            var second = world.scope("b");
            first.component(Scoped.class);
            second.component(Scoped.class);
            var failure = assertThrows(IllegalStateException.class, world.registry::close);
            assertInstanceOf(AssertionError.class, failure.getSuppressed()[0]);
            assertEquals(List.of("b", "a", "process"), events);
            assertThrows(IllegalStateException.class, () -> first.component(Scoped.class));
            assertDoesNotThrow(world.registry::close);
        }
    }

    private static <T> ComponentBinding<T> binding(
            Class<T> type,
            Component.Scope scope,
            List<Class<?>> dependencies,
            ComponentBinding.Factory<T> factory) {
        return new ComponentBinding<>(type, scope, dependencies, factory);
    }

    private static final class World implements AutoCloseable {
        final ComponentRegistry registry;
        final TickExecutor ticks = new TickExecutor();

        World(List<ComponentBinding<?>> bindings) {
            this(new ComponentRegistry(bindings));
        }

        World(ComponentRegistry registry) {
            this.registry = registry;
            MinecraftServer.init();
            ticks.flush();
        }

        SessionScope scope(String id) {
            return new SessionScope(
                    id, 1, ticks, () -> CompletableFuture.completedFuture(null), null, registry);
        }

        @Override
        public void close() {
            try {
                registry.close();
            } finally {
                MinecraftServer.process().stop();
            }
        }
    }

    private static class Shared implements AutoCloseable {
        final Runnable close;

        Shared(Runnable close) {
            this.close = close;
        }

        @Override
        public void close() {
            close.run();
        }
    }

    private static final class Scoped extends Shared {
        final Shared shared;

        Scoped(Shared shared, Runnable close) {
            super(close);
            this.shared = shared;
        }
    }

    private static final class Root extends Shared {
        final Scoped dependency;

        Root(Scoped dependency, Runnable close) {
            super(close);
            this.dependency = dependency;
        }
    }
}
