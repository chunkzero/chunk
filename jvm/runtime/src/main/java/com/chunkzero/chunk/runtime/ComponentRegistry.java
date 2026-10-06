package com.chunkzero.chunk.runtime;

import com.chunkzero.chunk.backend.client.BackendSession;

import java.util.ArrayList;
import java.util.Collection;
import java.util.HashMap;
import java.util.IdentityHashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.ServiceLoader;

/**
 * Owns generated factory instances for one app process and its independent sessions. Session
 * factories may depend on {@link BackendSession} and {@link Component.Supplied} types, which the
 * host supplies when it opens a session's components.
 */
public final class ComponentRegistry implements AutoCloseable {
    private final Map<Class<?>, ComponentBinding<?>> bindings = new HashMap<>();
    private final Store process = new Store(Map.of());
    private final List<SessionComponents> sessions = new ArrayList<>();
    private final Map<Object, Entry> owned = new IdentityHashMap<>();
    private boolean closed;
    private boolean changing;

    /**
     * Providers are generated from build-validated graphs (cycles, missing providers, limits), so
     * only what several providers or a lifetime bug could still break is checked here.
     */
    public ComponentRegistry(Collection<ComponentBinding<?>> declarations) {
        if (declarations.size() > 256) throw new IllegalArgumentException("Too many components");
        for (var binding : declarations) {
            if (builtin(binding.type()) || binding.type().isPrimitive() || binding.type().isArray())
                throw new IllegalArgumentException("Invalid component identity: " + binding.type());
            if (bindings.putIfAbsent(binding.type(), binding) != null)
                throw new IllegalArgumentException("Duplicate component: " + binding.type());
        }
        for (var binding : bindings.values()) {
            if (binding.scope() != Component.Scope.PROCESS) continue;
            for (var dependency : binding.dependencies()) {
                var target = bindings.get(dependency);
                if (builtin(dependency)
                        || target != null && target.scope() == Component.Scope.SESSION)
                    throw new IllegalArgumentException(
                            "Process component captures session: " + binding.type().getName());
            }
        }
    }

    /** Loads the build-generated {@link ComponentProvider} services. */
    public static ComponentRegistry load(ClassLoader loader) {
        var declarations = new ArrayList<ComponentBinding<?>>();
        for (var provider : ServiceLoader.load(ComponentProvider.class, loader)) {
            var components = provider.components();
            if (components.size() > 256 - declarations.size())
                throw new IllegalArgumentException("Too many components");
            declarations.addAll(components);
        }
        return new ComponentRegistry(declarations);
    }

    /**
     * Opens one session's components, given the values the host supplies by type.
     *
     * @throws IllegalArgumentException if a key is neither {@link BackendSession} nor a {@link
     *     Component.Supplied} type, or its value is not an instance of it
     */
    public synchronized SessionComponents session(Map<Class<?>, Object> supplied) {
        for (var entry : supplied.entrySet()) {
            if (!builtin(entry.getKey()) || !entry.getKey().isInstance(entry.getValue()))
                throw new IllegalArgumentException(
                        "Invalid supplied component: " + entry.getKey().getName());
        }
        checkActive();
        checkIdle();
        var components = new SessionComponents(new Store(Map.copyOf(supplied)));
        sessions.add(components);
        return components;
    }

    private Object resolve(Store session, Class<?> type, List<Entry> created) throws Exception {
        if (builtin(type)) {
            var supplied = session.supplied.get(type);
            if (supplied != null) return supplied;
            throw new IllegalStateException(
                    type == BackendSession.class
                            ? "Session backend unavailable"
                            : "Supplied component unavailable: " + type.getName());
        }
        var binding = bindings.get(type);
        if (binding == null)
            throw new IllegalArgumentException("Missing component: " + type.getName());
        var store = binding.scope() == Component.Scope.PROCESS ? process : session;
        var existing = store.values.get(type);
        if (existing != null) return existing.value;
        var dependencies = new Object[binding.dependencies().size()];
        for (int index = 0; index < dependencies.length; index++)
            dependencies[index] = resolve(store, binding.dependencies().get(index), created);
        var value =
                Objects.requireNonNull(
                        binding.factory().create(dependencies), "Component factory returned null");
        if (value instanceof BackendSession || builtin(value.getClass()))
            throw new IllegalStateException(
                    "Component factory returned a borrowed session capability");
        if (owned.containsKey(value))
            throw new IllegalStateException("Component factory returned an already owned resource");
        if (!type.isInstance(value)) {
            var failure =
                    new IllegalArgumentException("Component factory returned a different type");
            closeValue(value, failure);
            throw failure;
        }
        var entry = new Entry(store, type, value);
        store.values.put(type, entry);
        if (value instanceof AutoCloseable) owned.put(value, entry);
        created.add(entry);
        return value;
    }

    private static boolean builtin(Class<?> type) {
        return type == BackendSession.class || type.isAnnotationPresent(Component.Supplied.class);
    }

    private void checkActive() {
        if (closed) throw new IllegalStateException("Components closed");
    }

    private void checkIdle() {
        if (changing)
            throw new IllegalStateException(
                    "Reentrant component access; declare dependencies as factory parameters");
    }

    @Override
    public synchronized void close() {
        if (closed) return;
        checkIdle();
        changing = true;
        closed = true;
        try {
            var failure = new IllegalStateException("Component disposal failed");
            for (var session : List.copyOf(sessions).reversed()) {
                session.store.closed = true;
                release(new ArrayList<>(session.store.values.values()), failure);
            }
            sessions.clear();
            release(new ArrayList<>(process.values.values()), failure);
            if (failure.getSuppressed().length != 0) throw failure;
        } finally {
            changing = false;
        }
    }

    private void release(List<Entry> entries, Throwable failure) {
        for (var entry : entries.reversed()) {
            entry.store.values.remove(entry.type);
            owned.remove(entry.value);
            closeValue(entry.value, failure);
        }
    }

    private static void closeValue(Object value, Throwable failure) {
        if (value instanceof AutoCloseable resource) {
            try {
                resource.close();
            } catch (Exception | Error error) {
                if (error != failure) failure.addSuppressed(error);
            }
        }
    }

    /** One session's components; only the session's thread may access them. */
    public final class SessionComponents implements AutoCloseable {
        private final Store store;

        private SessionComponents(Store store) {
            this.store = store;
        }

        /** Resolves an exact declared component type, creating it and its dependencies once. */
        public <T> T get(Class<T> type) {
            synchronized (ComponentRegistry.this) {
                checkActive();
                if (store.closed) throw new IllegalStateException("Session components closed");
                checkIdle();
                changing = true;
                var created = new ArrayList<Entry>();
                try {
                    return type.cast(resolve(store, type, created));
                } catch (Exception | Error error) {
                    release(created, error);
                    if (error instanceof RuntimeException runtime) throw runtime;
                    if (error instanceof Error fatal) throw fatal;
                    throw new IllegalStateException(
                            "Component construction failed: " + type.getName(), error);
                } finally {
                    changing = false;
                }
            }
        }

        @Override
        public void close() {
            synchronized (ComponentRegistry.this) {
                if (store.closed) return;
                checkIdle();
                changing = true;
                store.closed = true;
                sessions.remove(this);
                try {
                    var failure = new IllegalStateException("Session component disposal failed");
                    release(new ArrayList<>(store.values.values()), failure);
                    if (failure.getSuppressed().length != 0) throw failure;
                } finally {
                    changing = false;
                }
            }
        }
    }

    private static final class Store {
        final Map<Class<?>, Object> supplied;
        final Map<Class<?>, Entry> values = new LinkedHashMap<>();
        boolean closed;

        Store(Map<Class<?>, Object> supplied) {
            this.supplied = supplied;
        }
    }

    private record Entry(Store store, Class<?> type, Object value) {}
}
