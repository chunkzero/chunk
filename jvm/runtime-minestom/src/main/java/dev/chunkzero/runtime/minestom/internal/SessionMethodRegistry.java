package dev.chunkzero.runtime.minestom.internal;

import dev.chunkzero.runtime.SessionMethodBinding;
import dev.chunkzero.runtime.SessionMethodProvider;

import org.jetbrains.annotations.ApiStatus;

import java.util.Collection;
import java.util.Map;
import java.util.ServiceLoader;
import java.util.TreeMap;

/** Loads only the build-generated, app-owned method provider registrations. */
@ApiStatus.Internal
public final class SessionMethodRegistry {
    private SessionMethodRegistry() {}

    public static Map<String, SessionMethodBinding<?, ?>> load(
            String app, Collection<String> sessions, ClassLoader loader) {
        var methods = new TreeMap<String, SessionMethodBinding<?, ?>>();
        for (var provider : ServiceLoader.load(SessionMethodProvider.class, loader)) {
            for (var binding : provider.methods()) {
                var ref = binding.reference();
                var session = app + "/" + ref.session();
                if (!ref.app().equals(app)
                        || !sessions.contains(session)
                        || !ref.name().matches("[A-Za-z_][A-Za-z0-9_]{0,127}")) {
                    throw new IllegalArgumentException("Foreign or invalid session method binding");
                }
                if (methods.put(session + "/" + ref.name(), binding) != null
                        || methods.size() > 256) {
                    throw new IllegalArgumentException(
                            "Duplicate or excessive session method bindings");
                }
            }
        }
        return Map.copyOf(methods);
    }
}
