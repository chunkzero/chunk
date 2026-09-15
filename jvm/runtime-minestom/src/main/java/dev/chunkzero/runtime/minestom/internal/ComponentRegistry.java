package dev.chunkzero.runtime.minestom.internal;

import dev.chunkzero.backend.client.BackendSession;
import dev.chunkzero.runtime.Component;
import dev.chunkzero.runtime.ComponentBinding;
import dev.chunkzero.runtime.ComponentProvider;
import dev.chunkzero.runtime.SessionScope;

import org.jetbrains.annotations.ApiStatus;
import org.jetbrains.annotations.Nullable;

import java.util.ArrayList;
import java.util.Collection;
import java.util.HashMap;
import java.util.IdentityHashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.ServiceLoader;

/** Owns generated factory instances for one app process and its independent sessions. */
@ApiStatus.Internal
public final class ComponentRegistry implements AutoCloseable {
    private final Map<Class<?>, ComponentBinding<?>> bindings = new HashMap<>();
    private final Store process = new Store(null);
    private final List<SessionComponents> sessions = new ArrayList<>();
    private final Map<Object, Entry> owned = new IdentityHashMap<>();
    private boolean closed;
    private boolean changing;

    public ComponentRegistry(Collection<ComponentBinding<?>> declarations) {
        if (declarations.size() > 256) throw new IllegalArgumentException("Too many components");
        for (var binding : declarations) {
            if (builtin(binding.type()) || binding.type().isPrimitive() || binding.type().isArray())
                throw new IllegalArgumentException("Invalid component identity: " + binding.type());
            if (bindings.putIfAbsent(binding.type(), binding) != null)
                throw new IllegalArgumentException("Duplicate component: " + binding.type());
        }
        var visited = new HashMap<Class<?>, Boolean>();
        for (var type : bindings.keySet()) validate(type, visited);
    }

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

    public synchronized SessionComponents session(SessionScope scope) {
        checkActive();
        checkIdle();
        var components = new SessionComponents(new Store(scope));
        sessions.add(components);
        return components;
    }

    private void validate(Class<?> type, Map<Class<?>, Boolean> visited) {
        var previous = visited.putIfAbsent(type, false);
        if (Boolean.TRUE.equals(previous)) return;
        if (Boolean.FALSE.equals(previous))
            throw new IllegalArgumentException("Component cycle: " + type.getName());
        var binding = bindings.get(type);
        if (binding == null)
            throw new IllegalArgumentException("Missing component: " + type.getName());
        if (binding.dependencies().size() > 32)
            throw new IllegalArgumentException("Too many component dependencies");
        for (var dependency : binding.dependencies()) {
            var target = bindings.get(dependency);
            if (binding.scope() == Component.Scope.PROCESS
                    && (builtin(dependency)
                            || target != null && target.scope() == Component.Scope.SESSION))
                throw new IllegalArgumentException(
                        "Process component captures session: " + type.getName());
            if (!builtin(dependency)) validate(dependency, visited);
        }
        visited.put(type, true);
    }

    private Object resolve(Store session, Class<?> type, List<Entry> created) throws Exception {
        if (type == SessionScope.class) return Objects.requireNonNull(session.scope);
        if (type == BackendSession.class)
            return Objects.requireNonNull(
                    Objects.requireNonNull(session.scope).getBackend(),
                    "Session backend unavailable");
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
        if (value instanceof SessionScope || value instanceof BackendSession)
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
        return type == SessionScope.class || type == BackendSession.class;
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

    /** SessionScope owns this cache; only the session tick thread may access its components. */
    public final class SessionComponents implements AutoCloseable {
        private final Store store;

        private SessionComponents(Store store) {
            this.store = store;
        }

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
        final @Nullable SessionScope scope;
        final Map<Class<?>, Entry> values = new LinkedHashMap<>();
        boolean closed;

        Store(@Nullable SessionScope scope) {
            this.scope = scope;
        }
    }

    private record Entry(Store store, Class<?> type, Object value) {}
}
