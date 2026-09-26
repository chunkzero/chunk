package dev.chunkzero.runtime;

import chunk.v1.Supervision.SessionInventory;

import dev.chunkzero.runtime.minestom.internal.AppRegistry;
import dev.chunkzero.runtime.minestom.internal.ComponentRegistry;
import dev.chunkzero.runtime.minestom.internal.GameplayService;
import dev.chunkzero.runtime.minestom.internal.ProcessService;
import dev.chunkzero.runtime.minestom.internal.SessionMethodRegistry;
import dev.chunkzero.runtime.minestom.internal.SessionMethodService;

import net.minestom.server.ServerProcess;
import net.minestom.server.timer.Task;
import net.minestom.server.timer.TaskSchedule;

import org.jetbrains.annotations.Nullable;

import java.io.IOException;
import java.net.InetSocketAddress;
import java.util.List;
import java.util.concurrent.atomic.AtomicBoolean;

/** Integrates an app-owned Minestom server with Chunk's generic process lifecycle. */
public final class ChunkMinestom implements AutoCloseable {
    private final ChunkProcess chunk;
    private final ServerProcess server;
    private final TickExecutor ticks = new TickExecutor();
    private final SessionManager sessions;
    private final ComponentRegistry components;
    private final GameplayService gameplay;
    private final ProcessService service;
    private final SessionMethodService methods;
    private final AtomicBoolean closed = new AtomicBoolean();
    private final Thread shutdownHook;
    private @Nullable Task task;
    private boolean started;

    private ChunkMinestom(ChunkProcess chunk, ServerProcess server) {
        this.chunk = chunk;
        this.server = server;
        var factories =
                AppRegistry.load(
                        chunk.identity().getAppId(),
                        Thread.currentThread().getContextClassLoader());
        components = ComponentRegistry.load(Thread.currentThread().getContextClassLoader());
        sessions =
                new SessionManager(
                        server,
                        ticks,
                        factories,
                        (session, appId) -> chunk.backend(session),
                        components);
        server.setCompressionThreshold(0);
        server.connectionManager().setPlayerProvider(ManagedPlayer::new);
        var identity = chunk.identity();
        gameplay =
                new GameplayService(
                        identity.getDeployment(),
                        identity.getGeneration(),
                        sessions,
                        System::nanoTime,
                        identity.getRuntimeId(),
                        chunk::isReady);
        methods =
                new SessionMethodService(
                        identity,
                        sessions,
                        SessionMethodRegistry.load(
                                identity.getAppId(),
                                factories.keySet(),
                                Thread.currentThread().getContextClassLoader()),
                        gameplay::authorizeMethodCaller,
                        System::currentTimeMillis);
        service = new ProcessService(identity, gameplay, sessions, chunk);
        shutdownHook = new Thread(this::close, "chunk-minestom-shutdown");
        Runtime.getRuntime().addShutdownHook(shutdownHook);
    }

    /** Attach before starting the listener. Closing this integration stops Minestom. */
    public static ChunkMinestom attach(ChunkProcess chunk, ServerProcess server) {
        try {
            return new ChunkMinestom(chunk, server);
        } catch (RuntimeException | Error error) {
            server.stop();
            throw error;
        }
    }

    public synchronized void start() throws IOException {
        if (started || closed.get())
            throw new IllegalStateException("Server already started or closed");
        try {
            task =
                    server.schedulerManager()
                            .buildTask(
                                    () -> {
                                        methods.flush();
                                        ticks.flush();
                                        gameplay.flush();
                                        chunk.flush();
                                        var inventory = sessions.inventory();
                                        chunk.progress(
                                                sessions.activeCount(),
                                                inventory.stream()
                                                        .mapToInt(SessionInventory::getAttached)
                                                        .sum());
                                    })
                            .repeat(TaskSchedule.tick(1))
                            .schedule();
            server.start(new InetSocketAddress("127.0.0.1", 0));
            gameplay.setEndpoint("127.0.0.1:" + server.server().getPort());
            chunk.bind(List.of(gameplay, methods), gameplay.getEndpoint(), service);
            started = true;
        } catch (IOException | RuntimeException error) {
            close();
            throw error;
        }
    }

    @Override
    public synchronized void close() {
        if (!closed.compareAndSet(false, true)) return;
        Throwable failure = null;
        List<Runnable> cleanup =
                List.of(
                        server::stop,
                        () -> {
                            if (task != null) task.cancel();
                        },
                        methods::close,
                        gameplay::close,
                        components::close,
                        this::removeShutdownHook);
        for (var action : cleanup) {
            try {
                action.run();
            } catch (RuntimeException | Error error) {
                if (failure == null) failure = error;
                else if (failure != error) failure.addSuppressed(error);
            }
        }
        if (failure instanceof RuntimeException runtime) throw runtime;
        if (failure instanceof Error fatal) throw fatal;
    }

    private void removeShutdownHook() {
        if (Thread.currentThread() != shutdownHook) {
            try {
                Runtime.getRuntime().removeShutdownHook(shutdownHook);
            } catch (IllegalStateException ignored) {
                /* JVM shutdown already started. */
            }
        }
    }
}
