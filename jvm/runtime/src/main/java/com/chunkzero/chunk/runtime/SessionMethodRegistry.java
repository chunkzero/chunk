package com.chunkzero.chunk.runtime;

import java.util.ArrayList;
import java.util.Collection;
import java.util.Map;
import java.util.ServiceLoader;
import java.util.TreeMap;

/** An app's session methods by qualified session type ({@code app/type}) and method name. */
public final class SessionMethodRegistry {
    private final Map<String, SessionMethodBinding<?, ?, ?>> methods;

    /**
     * Registers {@code bindings}, each of which must belong to one of the qualified session {@code
     * types}.
     */
    public SessionMethodRegistry(
            Collection<String> types,
            Collection<? extends SessionMethodBinding<?, ?, ?>> bindings) {
        var methods = new TreeMap<String, SessionMethodBinding<?, ?, ?>>();
        for (var binding : bindings) {
            var ref = binding.reference();
            var type = ref.app() + "/" + ref.session();
            if (!types.contains(type) || !ref.name().matches("[A-Za-z_][A-Za-z0-9_]{0,127}"))
                throw new IllegalArgumentException("Foreign or invalid session method binding");
            if (methods.put(type + "/" + ref.name(), binding) != null || methods.size() > 256)
                throw new IllegalArgumentException(
                        "Duplicate or excessive session method bindings");
        }
        this.methods = Map.copyOf(methods);
    }

    /** Loads the build-generated {@link SessionMethodProvider} services of {@code app}. */
    public static SessionMethodRegistry load(
            String app, Collection<String> types, ClassLoader loader) {
        var bindings = new ArrayList<SessionMethodBinding<?, ?, ?>>();
        for (var provider : ServiceLoader.load(SessionMethodProvider.class, loader)) {
            for (var binding : provider.methods()) {
                if (!binding.reference().app().equals(app))
                    throw new IllegalArgumentException("Foreign or invalid session method binding");
                bindings.add(binding);
            }
        }
        return new SessionMethodRegistry(types, bindings);
    }

    /** Whether sessions of {@code type} declare {@code method}. */
    public boolean declares(String type, String method) {
        return methods.containsKey(type + "/" + method);
    }

    /**
     * Calls {@code method} on {@code session}, a session of {@code type}, and returns its result as
     * JSON.
     *
     * @throws IllegalArgumentException if the method is undeclared
     * @throws ClassCastException if {@code session} is not of the method's session class
     */
    public String invoke(String type, String method, Object session, String argumentsJson) {
        var binding = methods.get(type + "/" + method);
        if (binding == null) throw new IllegalArgumentException("Undeclared session method");
        return binding.invoke(session, argumentsJson);
    }
}
