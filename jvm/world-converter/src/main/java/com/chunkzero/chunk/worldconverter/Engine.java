package com.chunkzero.chunk.worldconverter;

import net.minestom.server.MinecraftServer;
import net.minestom.server.ServerProcess;
import net.minestom.server.exception.ExceptionHandler;
import net.minestom.server.exception.ExceptionManager;
import net.minestom.server.instance.InstanceContainer;
import net.minestom.server.registry.RegistryKey;
import net.minestom.server.world.DimensionType;

import java.lang.reflect.InvocationTargetException;
import java.util.UUID;

/**
 * A server process of the Minestom on the classpath. Upstream Minestom has one global process,
 * started through {@link MinecraftServer}; multistom creates processes with {@code
 * ServerProcess.create()} and passes them to instances. This is compiled against upstream, so
 * multistom's API is called reflectively.
 */
final class Engine implements AutoCloseable {
    // Read at run time: javac inlines Minestom's constants, and only multistom's class is public.
    static final int DATA_VERSION = (Integer) constant("DATA_VERSION");
    static final String VERSION_NAME = (String) constant("VERSION_NAME");

    private final ServerProcess process;
    private final boolean multistom;

    Engine(ExceptionHandler handler) {
        multistom = hasMethod("create");
        process =
                multistom
                        ? (ServerProcess) call(ServerProcess.class, "create", null)
                        : MinecraftServer.updateProcess();
        var exceptions =
                multistom
                        ? (ExceptionManager) call(ServerProcess.class, "exceptionManager", process)
                        : process.exception();
        exceptions.setExceptionHandler(handler);
    }

    ServerProcess process() {
        return process;
    }

    InstanceContainer overworld() {
        if (!multistom)
            return new InstanceContainer(process, UUID.randomUUID(), DimensionType.OVERWORLD);
        try {
            return InstanceContainer.class
                    .getConstructor(ServerProcess.class, UUID.class, RegistryKey.class)
                    .newInstance(process, UUID.randomUUID(), DimensionType.OVERWORLD);
        } catch (InvocationTargetException error) {
            throw rethrow(error);
        } catch (ReflectiveOperationException error) {
            throw new IllegalStateException("Unsupported Minestom version", error);
        }
    }

    @Override
    public void close() {
        process.stop();
    }

    private static Object constant(String name) {
        try {
            var field = Class.forName("net.minestom.server.MinecraftConstants").getField(name);
            field.setAccessible(true);
            return field.get(null);
        } catch (ReflectiveOperationException error) {
            throw new IllegalStateException("Unsupported Minestom version", error);
        }
    }

    private static boolean hasMethod(String name) {
        try {
            ServerProcess.class.getMethod(name);
            return true;
        } catch (NoSuchMethodException error) {
            return false;
        }
    }

    private static Object call(Class<?> type, String method, Object target) {
        try {
            return type.getMethod(method).invoke(target);
        } catch (InvocationTargetException error) {
            throw rethrow(error);
        } catch (ReflectiveOperationException error) {
            throw new IllegalStateException("Unsupported Minestom version", error);
        }
    }

    private static RuntimeException rethrow(InvocationTargetException error) {
        if (error.getCause() instanceof RuntimeException cause) return cause;
        if (error.getCause() instanceof Error cause) throw cause;
        return new IllegalStateException(error.getCause());
    }
}
