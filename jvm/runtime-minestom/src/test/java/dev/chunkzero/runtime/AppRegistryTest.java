package dev.chunkzero.runtime;

import static org.junit.jupiter.api.Assertions.*;

import dev.chunkzero.runtime.bootstrap.AppRegistry;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

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

class AppRegistryTest {
    private static final String APP_MANIFEST = "META-INF/chunk/app.json";
    private static final String SESSION_PROVIDER =
            "META-INF/services/dev.chunkzero.runtime.SessionProvider";
    @TempDir Path directory;

    @Test
    void generatedCatalogCreatesIndependentSessionsWithFixedCallerIdentity() throws Exception {
        var lobby = compile("lobby");
        var jar = jar("lobby.jar", lobby.classes(), "lobby", lobby.provider());
        try (var loader = loader(jar)) {
            var apps = AppRegistry.load(loader);
            assertEquals(List.of("lobby/default"), new ArrayList<>(apps.keySet()));
            var factory = apps.get("lobby/default");
            var first = factory.create();
            assertEquals("lobby", first.toString());
            assertNotSame(first, factory.create());
            factory.backend(
                    "instance",
                    (session, app) -> {
                        assertEquals("instance", session);
                        assertEquals("lobby", app);
                        return null;
                    });
        }
    }

    @Test
    void missingDuplicateAndInvalidDeclarationsFailBeforeCreation() throws Exception {
        var lobby = compile("lobby");
        var valid = jar("valid.jar", lobby.classes(), "lobby", lobby.provider());
        assertInvalid("exactly one", jar("orphan.jar", lobby.classes(), null, null));
        assertInvalid(
                "exactly one", valid, jar("duplicate.jar", Map.of(), "other", lobby.provider()));
        assertInvalid(
                "Invalid session provider",
                jar("absent.jar", Map.of(), "lobby", "missing.Provider"));
    }

    private void assertInvalid(String message, Path... jars) throws Exception {
        try (var loader = loader(jars)) {
            var error =
                    assertThrows(IllegalArgumentException.class, () -> AppRegistry.load(loader));
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
        Files.writeString(
                source,
                """
                package fixtures.%s;
                import dev.chunkzero.runtime.Session;
                import dev.chunkzero.runtime.SessionProvider;
                public final class Provider implements SessionProvider {
                    public Session create() {
                        return new Session() { public String toString() { return "%s"; } };
                    }
                }
                """
                        .formatted(app, app));
        var classes = Files.createDirectory(root.resolve("output"));
        var compiler = ToolProvider.getSystemJavaCompiler();
        assertNotNull(compiler, "App registry fixtures require the configured JDK");
        var runtime =
                Path.of(Session.class.getProtectionDomain().getCodeSource().getLocation().toURI());
        var classpath = System.getProperty("java.class.path") + File.pathSeparator + runtime;
        var diagnostics = new StringWriter();
        try (var files = compiler.getStandardFileManager(null, null, StandardCharsets.UTF_8)) {
            var success =
                    compiler.getTask(
                                    diagnostics,
                                    files,
                                    null,
                                    List.of(
                                            "-classpath",
                                            classpath,
                                            "-d",
                                            classes.toString(),
                                            "-proc:none"),
                                    null,
                                    files.getJavaFileObjects(source))
                            .call();
            assertTrue(success, diagnostics.toString());
        }
        var entries = new TreeMap<String, byte[]>();
        try (var paths = Files.walk(classes)) {
            for (var path : paths.filter(Files::isRegularFile).toList()) {
                entries.put(
                        classes.relativize(path).toString().replace(File.separatorChar, '/'),
                        Files.readAllBytes(path));
            }
        }
        return new Fixture("fixtures." + app + ".Provider", entries);
    }

    private Path jar(String name, Map<String, byte[]> classes, String app, String provider)
            throws IOException {
        var entries = new TreeMap<>(classes);
        if (app != null)
            entries.put(APP_MANIFEST, manifest(app, provider).getBytes(StandardCharsets.UTF_8));
        if (provider != null)
            entries.put(SESSION_PROVIDER, (provider + "\n").getBytes(StandardCharsets.UTF_8));
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

    private static String manifest(String app, String provider) {
        return """
        {"version":2,"id":"%s","main_class":"test.Main","sessions":{
          "default":{"provider":"%s","machine_profile":"small","capacity":16}}}
        """
                .formatted(app, provider);
    }

    private record Fixture(String provider, Map<String, byte[]> classes) {}
}
