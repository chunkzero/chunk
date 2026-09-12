package dev.chunkzero.runtime;

import static org.junit.jupiter.api.Assertions.*;

import dev.chunkzero.runtime.bootstrap.AppManifest;
import dev.chunkzero.runtime.minestom.internal.AppRegistry;

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
    @TempDir Path directory;

    @Test
    void generatedCatalogCreatesIndependentSessionsWithFixedCallerIdentity() throws Exception {
        var lobby = compile("lobby");
        var jar = jar("lobby.jar", lobby.classes());
        try (var loader = loader(jar)) {
            var apps = AppRegistry.load(manifest("lobby", lobby.provider()), loader);
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
    void missingAndInvalidProvidersFailBeforeCreation() {
        for (var provider : List.of("missing.Provider", "java.lang.String")) {
            var error =
                    assertThrows(
                            IllegalArgumentException.class,
                            () ->
                                    AppRegistry.load(
                                            manifest("lobby", provider),
                                            getClass().getClassLoader()));
            assertTrue(error.getMessage().contains("Invalid session provider"), error.toString());
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

    private Path jar(String name, Map<String, byte[]> classes) throws IOException {
        var path = directory.resolve(name);
        try (var output = new JarOutputStream(Files.newOutputStream(path))) {
            for (var entry : classes.entrySet()) {
                output.putNextEntry(new JarEntry(entry.getKey()));
                output.write(entry.getValue());
                output.closeEntry();
            }
        }
        return path;
    }

    private static AppManifest manifest(String app, String provider) {
        return new AppManifest(
                2,
                app,
                "test.Main",
                Map.of("default", new AppManifest.Factory(provider, "small", 16)));
    }

    private record Fixture(String provider, Map<String, byte[]> classes) {}
}
