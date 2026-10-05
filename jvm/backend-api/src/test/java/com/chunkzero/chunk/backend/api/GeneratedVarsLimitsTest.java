package com.chunkzero.chunk.backend.api;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

import tools.jackson.databind.node.ObjectNode;

import java.io.StringWriter;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.Optional;

import javax.tools.ToolProvider;

class GeneratedVarsLimitsTest {
    @Test
    void varsAtTheManifestLimitsCompileAndResolve(@TempDir Path root) throws Exception {
        // 64 KiB of 4-byte characters, and 256 variables overridden in each of 64 environments.
        var large = "😀".repeat(16 * 1024);
        var mapper = BackendJson.mapper();
        ObjectNode contract;
        try (var stream = getClass().getResourceAsStream("/contract.json")) {
            assertNotNull(stream);
            contract = (ObjectNode) mapper.readTree(stream.readAllBytes());
        }
        var env = contract.putObject("env");
        var vars = env.putObject("vars");
        var environments = env.putObject("environments");
        for (var index = 0; index < 64; index++) {
            var name = index == 0 ? "prod" : "e" + index;
            var overrides = environments.putObject(name);
            for (var variable = 0; variable < 256; variable++) {
                overrides.put("V" + variable, name + "-" + variable);
            }
        }
        for (var variable = 0; variable < 128; variable++) {
            vars.put("V" + variable, "default-" + variable);
        }
        ((ObjectNode) environments.get("prod")).put("V0", large).put("V255", large);
        var path =
                Files.writeString(
                        root.resolve("contract.json"), mapper.writeValueAsString(contract));

        var generated = root.resolve("generated");
        var codegen =
                new ProcessBuilder(
                                "cargo",
                                "run",
                                "-q",
                                "-p",
                                "chunk-build",
                                "--bin",
                                "chunk-codegen",
                                "--",
                                "java",
                                path.toString(),
                                generated.toString(),
                                "com.chunkzero.chunk.limits")
                        .directory(Path.of(System.getProperty("chunk.root")).toFile())
                        .redirectErrorStream(true)
                        .start();
        var output = new String(codegen.getInputStream().readAllBytes(), StandardCharsets.UTF_8);
        assertEquals(0, codegen.waitFor(), output);

        var classes = Files.createDirectory(root.resolve("classes"));
        var compiler = ToolProvider.getSystemJavaCompiler();
        assertNotNull(compiler, "Vars compilation requires the configured JDK");
        var diagnostics = new StringWriter();
        try (var files = compiler.getStandardFileManager(null, null, StandardCharsets.UTF_8)) {
            var source = generated.resolve("java/com/chunkzero/chunk/limits/Vars.java");
            var options = List.of("-Xlint:all", "-Werror", "-d", classes.toString());
            var task =
                    compiler.getTask(
                            diagnostics,
                            files,
                            null,
                            options,
                            null,
                            files.getJavaFileObjects(source));
            assertTrue(task.call(), diagnostics.toString());
        }
        // The test task sets CHUNK_ENVIRONMENT_NAME=prod.
        try (var loader = new URLClassLoader(new URL[] {classes.toUri().toURL()})) {
            var type = loader.loadClass("com.chunkzero.chunk.limits.Vars");
            assertEquals(large, type.getField("V0").get(null));
            assertEquals("prod-1", type.getField("V1").get(null));
            assertEquals(Optional.of(large), type.getField("V255").get(null));
        }
    }
}
