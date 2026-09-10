package dev.chunkzero.runtime;

import java.io.File;
import java.io.IOException;
import java.io.StringWriter;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;
import javax.tools.ToolProvider;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import static org.junit.jupiter.api.Assertions.*;

class AppRegistryTest {
    @TempDir Path directory;

    @Test void originatingJarsSelectIndependentSessionsAndPreserveCallerAppIdentity() throws Exception {
        var lobby = compile("lobby");
        var arena = compile("arena");
        var lobbyJar = jar("lobby.jar", lobby.classes(), "lobby", lobby.provider());
        var arenaJar = jar("arena.jar", arena.classes(), "arena", arena.provider());
        try (var loader = loader(lobbyJar, arenaJar)) {
            var apps = AppRegistry.load(loader);
            assertEquals(List.of("arena", "lobby"), new ArrayList<>(apps.keySet()));
            var first = apps.get("lobby").create();
            assertEquals("lobby", first.toString());
            assertNotSame(first, apps.get("lobby").create());
            assertEquals("arena", apps.get("arena").create().toString());
            var identities = new ArrayList<List<String>>();
            for (var app : apps.values()) {
                app.backend("instance-" + app.appId(), (session, callerApp) -> {
                    identities.add(List.of(session, callerApp));
                    return null;
                });
            }
            assertEquals(List.of(List.of("instance-arena", "arena"), List.of("instance-lobby", "lobby")), identities);
        }
    }

    @Test void missingDuplicateAndForeignRegistrationsFailBeforeCreatingProviders() throws Exception {
        var lobby = compile("lobby");
        var arena = compile("arena");
        var valid = jar("valid.jar", lobby.classes(), "lobby", lobby.provider());
        assertInvalid("Missing or duplicate", jar("missing.jar", Map.of(), "lobby", null));
        assertInvalid("no app manifest", jar("orphan.jar", lobby.classes(), null, lobby.provider()));
        assertInvalid("exactly one", jar("multiple.jar", lobby.classes(), "lobby", lobby.provider() + "\n" + arena.provider()));
        assertInvalid("Duplicate app ID", valid, jar("duplicate.jar", arena.classes(), "LOBBY", arena.provider()));
        assertInvalid("Duplicate session provider", valid, jar("repeated.jar", Map.of(), "arena", lobby.provider()));
        assertInvalid("different JAR",
                jar("foreign-app.jar", Map.of(), "lobby", lobby.provider()),
                jar("dependency.jar", lobby.classes(), null, null));
        assertInvalid("Invalid app session provider", jar("absent-class.jar", Map.of(), "lobby", "missing.Provider"));
        var exploded = directory.resolve("exploded");
        Files.createDirectories(exploded.resolve("META-INF/chunk"));
        Files.writeString(exploded.resolve(AppRegistry.MANIFEST), manifest("lobby"));
        assertInvalid("packaged JARs", exploded);
    }

    private void assertInvalid(String message, Path... jars) throws Exception {
        try (var loader = loader(jars)) {
            var error = assertThrows(IllegalArgumentException.class, () -> AppRegistry.load(loader));
            assertTrue(error.getMessage().contains(message), error.toString());
        }
    }

    private static URLClassLoader loader(Path... jars) throws IOException {
        var urls = new URL[jars.length];
        for (var index = 0; index < jars.length; index++) urls[index] = jars[index].toUri().toURL();
        return new URLClassLoader(urls, SessionProvider.class.getClassLoader());
    }

    private Fixture compile(String app) throws Exception {
        var root = Files.createTempDirectory(directory, "classes-");
        var source = root.resolve("Provider.java");
        Files.writeString(source, """
                package fixtures.%s;
                import dev.chunkzero.runtime.Session;
                import dev.chunkzero.runtime.SessionProvider;
                public final class Provider implements SessionProvider {
                    public Session create() {
                        return new Session() { public String toString() { return "%s"; } };
                    }
                }
                """.formatted(app, app));
        var classes = Files.createDirectory(root.resolve("output"));
        var compiler = ToolProvider.getSystemJavaCompiler();
        assertNotNull(compiler, "App registry fixtures require the configured JDK");
        var runtime = Path.of(Session.class.getProtectionDomain().getCodeSource().getLocation().toURI());
        var classpath = System.getProperty("java.class.path") + File.pathSeparator + runtime;
        var diagnostics = new StringWriter();
        try (var files = compiler.getStandardFileManager(null, null, StandardCharsets.UTF_8)) {
            var success = compiler.getTask(diagnostics, files, null,
                    List.of("-classpath", classpath, "-d", classes.toString(), "-proc:none"), null,
                    files.getJavaFileObjects(source)).call();
            assertTrue(success, diagnostics.toString());
        }
        var entries = new TreeMap<String, byte[]>();
        try (var paths = Files.walk(classes)) {
            for (var path : paths.filter(Files::isRegularFile).toList()) {
                entries.put(classes.relativize(path).toString().replace(File.separatorChar, '/'), Files.readAllBytes(path));
            }
        }
        return new Fixture("fixtures." + app + ".Provider", entries);
    }

    private Path jar(String name, Map<String, byte[]> classes, String app, String provider) throws IOException {
        var entries = new TreeMap<>(classes);
        if (app != null) entries.put(AppRegistry.MANIFEST, manifest(app).getBytes(StandardCharsets.UTF_8));
        if (provider != null) entries.put(AppRegistry.SERVICE, (provider + "\n").getBytes(StandardCharsets.UTF_8));
        var path = directory.resolve(name);
        try (var output = new JarOutputStream(Files.newOutputStream(path))) {
            for (var entry : entries.entrySet()) {
                output.putNextEntry(new JarEntry(entry.getKey()));
                output.write(entry.getValue());
                output.closeEntry();
            }
        }
        return path;
    }

    private static String manifest(String app) {
        return "{\"version\":1,\"id\":\"" + app + "\"}";
    }

    private record Fixture(String provider, Map<String, byte[]> classes) {}
}
