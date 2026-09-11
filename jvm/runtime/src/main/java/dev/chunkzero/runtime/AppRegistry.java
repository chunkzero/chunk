package dev.chunkzero.runtime;

import dev.chunkzero.backend.api.Codecs;

import java.io.IOException;
import java.net.JarURLConnection;
import java.net.URISyntaxException;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.util.Collections;
import java.util.HashMap;
import java.util.HashSet;
import java.util.Locale;
import java.util.Map;
import java.util.ServiceConfigurationError;
import java.util.ServiceLoader;
import java.util.Set;
import java.util.TreeMap;
import java.util.jar.JarFile;

/** Loads registrations from their originating app JARs on the shared gameplay classpath. */
final class AppRegistry {
    static final String MANIFEST = "META-INF/chunk/app.json";
    static final String SERVICE = "META-INF/services/" + SessionProvider.class.getName();
    private static final int RESOURCE_LIMIT = 65_536;

    private AppRegistry() {}

    static Map<String, SessionRegistration> load(ClassLoader loader) throws IOException {
        var apps = manifests(loader);
        var origins = new HashSet<Path>();
        apps.values().forEach(app -> origins.add(app.jar()));
        for (var resource : Collections.list(loader.getResources(SERVICE))) {
            var jar = origin(resource);
            if (!origins.contains(jar)) throw invalid(jar, "Session provider has no app manifest");
        }
        var services = providers(loader);
        var selected = new TreeMap<String, ServiceLoader.Provider<SessionProvider>>();
        for (var app : apps.values()) {
            var service = services.remove(app.provider());
            if (service == null)
                throw invalid(app.jar(), "Missing or duplicate provider: " + app.provider());
            var source = service.type().getProtectionDomain().getCodeSource();
            if (source == null || !localPath(source.getLocation()).equals(app.jar())) {
                throw invalid(
                        app.jar(), "Provider " + app.provider() + " belongs to a different JAR");
            }
            selected.put(app.id(), service);
        }
        if (!services.isEmpty())
            throw new IllegalArgumentException(
                    "Unregistered session providers: " + services.keySet());
        var registrations = new TreeMap<String, SessionRegistration>();
        for (var entry : selected.entrySet()) {
            try {
                var provider = entry.getValue().get();
                registrations.put(
                        entry.getKey(), new SessionRegistration(entry.getKey(), provider::create));
            } catch (ServiceConfigurationError error) {
                throw new IllegalArgumentException(
                        "Cannot create session provider for app " + entry.getKey(), error);
            }
        }
        return Collections.unmodifiableMap(registrations);
    }

    private static Map<String, App> manifests(ClassLoader loader) throws IOException {
        var apps = new TreeMap<String, App>();
        var identities = new HashSet<String>();
        var providers = new HashSet<String>();
        var origins = new HashSet<Path>();
        for (var resource : Collections.list(loader.getResources(MANIFEST))) {
            var path = origin(resource);
            if (!origins.add(path)) throw invalid(path, "Duplicate app manifest resource");
            try (var jar = new JarFile(path.toFile())) {
                var id = appId(jar);
                if (!identities.add(id.toLowerCase(Locale.ROOT)))
                    throw invalid(path, "Duplicate app ID: " + id);
                var entries =
                        read(jar, SERVICE)
                                .lines()
                                .map(line -> line.split("#", 2)[0].trim())
                                .filter(line -> !line.isEmpty())
                                .toList();
                if (entries.size() != 1)
                    throw invalid(path, "App requires exactly one session provider");
                var provider = entries.getFirst();
                if (!providers.add(provider))
                    throw invalid(path, "Duplicate session provider: " + provider);
                apps.put(id, new App(id, path, provider));
            }
        }
        return apps;
    }

    private static String appId(JarFile jar) throws IOException {
        try {
            var manifest =
                    Codecs.object(Codecs.parse(read(jar, MANIFEST)), Set.of("version", "id"));
            if (Codecs.field(manifest, "version", Codecs.INTEGER) != 1L) {
                throw new IllegalArgumentException("Unsupported app manifest version");
            }
            var id = Codecs.field(manifest, "id", Codecs.STRING);
            if (!id.matches("[A-Za-z_][A-Za-z0-9_]{0,127}"))
                throw new IllegalArgumentException("Invalid app ID: " + id);
            return id;
        } catch (IllegalArgumentException error) {
            throw invalid(
                    Path.of(jar.getName()), "Invalid app manifest: " + error.getMessage(), error);
        }
    }

    private static Map<String, ServiceLoader.Provider<SessionProvider>> providers(
            ClassLoader loader) {
        var providers = new HashMap<String, ServiceLoader.Provider<SessionProvider>>();
        try {
            for (var provider :
                    ServiceLoader.load(SessionProvider.class, loader).stream().toList()) {
                providers.put(provider.type().getName(), provider);
            }
        } catch (ServiceConfigurationError | LinkageError error) {
            throw new IllegalArgumentException(
                    "Invalid app session provider: " + error.getMessage(), error);
        }
        return providers;
    }

    private static Path origin(URL resource) throws IOException {
        if (!(resource.openConnection() instanceof JarURLConnection connection)) {
            throw new IllegalArgumentException(
                    "App registration requires packaged JARs: " + resource);
        }
        return localPath(connection.getJarFileURL());
    }

    private static Path localPath(URL location) throws IOException {
        if (!location.getProtocol().equals("file")) {
            throw new IllegalArgumentException(
                    "App registration requires a local JAR: " + location);
        }
        try {
            return Path.of(location.toURI()).toRealPath();
        } catch (URISyntaxException error) {
            throw new IllegalArgumentException("Invalid app JAR location: " + location, error);
        }
    }

    private static String read(JarFile jar, String name) throws IOException {
        if (jar.stream().filter(entry -> entry.getName().equals(name)).count() != 1) {
            throw invalid(Path.of(jar.getName()), "Missing or duplicate " + name);
        }
        var entry = jar.getJarEntry(name);
        if (entry.isDirectory()) throw invalid(Path.of(jar.getName()), "Expected file: " + name);
        try (var stream = jar.getInputStream(entry)) {
            var bytes = stream.readNBytes(RESOURCE_LIMIT + 1);
            if (bytes.length > RESOURCE_LIMIT)
                throw invalid(Path.of(jar.getName()), "Registration resource too large: " + name);
            return new String(bytes, StandardCharsets.UTF_8);
        }
    }

    private static IllegalArgumentException invalid(Path jar, String message) {
        return new IllegalArgumentException(jar + ": " + message);
    }

    private static IllegalArgumentException invalid(Path jar, String message, Throwable cause) {
        return new IllegalArgumentException(jar + ": " + message, cause);
    }

    private record App(String id, Path jar, String provider) {}
}
