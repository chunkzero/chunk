package com.chunkzero.chunk.runtime;

import static org.junit.jupiter.api.Assertions.*;

import com.chunkzero.chunk.backend.api.JsonType;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

import tools.jackson.core.type.TypeReference;

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
import java.util.Objects;
import java.util.TreeMap;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;

import javax.tools.ToolProvider;

class SessionRegistryTest {
    @TempDir Path directory;

    @Test
    void serviceRegistryCreatesIndependentSessions() throws Exception {
        var lobby = compile("lobby");
        var jar = jar("lobby.jar", lobby.classes(), lobby.provider());
        try (var loader = loader(jar)) {
            var registry = SessionRegistry.load("lobby", loader);
            assertEquals(List.of("lobby/default"), new ArrayList<>(registry.types()));
            var first = registry.create("lobby/default", 1, "{}");
            assertEquals("lobby", first.toString());
            assertNotSame(first, registry.create("lobby/default", 1, "{}"));
            assertTrue(SessionRegistry.load("renamed", loader).types().contains("renamed/default"));
        }
    }

    @Test
    void missingAndInvalidProvidersFailBeforeCreation() throws Exception {
        for (var provider : List.of("missing.Provider", "java.lang.String")) {
            try (var loader = loader(jar(provider + ".jar", Map.of(), provider))) {
                assertThrows(
                        IllegalArgumentException.class,
                        () -> SessionRegistry.load("lobby", loader));
            }
        }
        try (var loader = loader(jar("empty.jar", Map.of(), ""))) {
            assertThrows(
                    IllegalArgumentException.class, () -> SessionRegistry.load("lobby", loader));
        }
        var first = compile("first");
        var second = compile("second");
        var classes = new TreeMap<>(first.classes());
        classes.putAll(second.classes());
        try (var loader =
                loader(
                        jar(
                                "duplicate.jar",
                                classes,
                                first.provider() + "\n" + second.provider()))) {
            var error =
                    assertThrows(
                            IllegalArgumentException.class,
                            () -> SessionRegistry.load("lobby", loader));
            assertTrue(error.getMessage().contains("Duplicate session type"), error.toString());
        }
    }

    @Test
    void configuredProvidersDecodeTheirConfigAndPlainOnesRequireAnEmptyOne() {
        var provider =
                new ConfiguredSessionProvider<Config, SessionCreation<Config>>() {
                    @Override
                    public JsonType<Config> configurationType() {
                        return JsonType.of(new TypeReference<Config>() {}, Objects::requireNonNull);
                    }

                    @Override
                    public SessionCreation<Config> create(SessionCreation<Config> creation) {
                        return creation;
                    }
                };
        var registry =
                new SessionRegistry(
                        Map.of(
                                "arena/koth",
                                provider,
                                "arena/plain",
                                (SessionProvider<String>) () -> "plain"));
        assertEquals(
                new SessionCreation<>(16, new Config("forest")),
                registry.create("arena/koth", 16, "{\"map\":\"forest\"}"));
        for (var json : List.of("{}", "null", "{\"map\":1}", "{\"map\":\"a\",\"x\":1}"))
            assertThrows(RuntimeException.class, () -> registry.create("arena/koth", 16, json));
        assertThrows(
                IllegalArgumentException.class,
                () -> registry.create("arena/koth", 0, "{\"map\":\"forest\"}"));
        assertEquals("plain", registry.create("arena/plain", 16, "{}"));
        for (var json : List.of("{\"map\":\"forest\"}", "[]", "null"))
            assertThrows(
                    IllegalArgumentException.class, () -> registry.create("arena/plain", 16, json));
        assertThrows(
                IllegalArgumentException.class, () -> registry.create("arena/missing", 16, "{}"));
    }

    public record Config(String map) {
        public Config {
            Objects.requireNonNull(map);
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
                import com.chunkzero.chunk.runtime.SessionProvider;
                @com.chunkzero.chunk.runtime.SessionType("default")
                public final class Provider implements SessionProvider<Object> {
                    public Object create() {
                        return new Object() { public String toString() { return "%s"; } };
                    }
                }
                """
                        .formatted(app, app));
        var classes = Files.createDirectory(root.resolve("output"));
        var compiler = ToolProvider.getSystemJavaCompiler();
        assertNotNull(compiler, "App registry fixtures require the configured JDK");
        var runtime =
                Path.of(
                        SessionType.class
                                .getProtectionDomain()
                                .getCodeSource()
                                .getLocation()
                                .toURI());
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

    private Path jar(String name, Map<String, byte[]> classes, String providers)
            throws IOException {
        var path = directory.resolve(name);
        try (var output = new JarOutputStream(Files.newOutputStream(path))) {
            output.putNextEntry(
                    new JarEntry("META-INF/services/com.chunkzero.chunk.runtime.SessionProvider"));
            output.write(providers.getBytes(StandardCharsets.UTF_8));
            output.closeEntry();
            for (var entry : classes.entrySet()) {
                output.putNextEntry(new JarEntry(entry.getKey()));
                output.write(entry.getValue());
                output.closeEntry();
            }
        }
        return path;
    }

    private record Fixture(String provider, Map<String, byte[]> classes) {}
}
