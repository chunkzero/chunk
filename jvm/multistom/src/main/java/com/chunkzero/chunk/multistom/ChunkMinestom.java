package com.chunkzero.chunk.multistom;

import com.chunkzero.chunk.multistom.internal.GameplayService;
import com.chunkzero.chunk.runtime.ChunkProcess;
import com.chunkzero.chunk.runtime.ChunkSessions;
import com.chunkzero.chunk.runtime.ComponentRegistry;
import com.chunkzero.chunk.runtime.SessionMethodRegistry;
import com.chunkzero.chunk.runtime.SessionRegistry;

import net.minestom.server.MinecraftConstants;
import net.minestom.server.ServerProcess;
import net.minestom.server.timer.Task;
import net.minestom.server.timer.TaskSchedule;

import org.jetbrains.annotations.Nullable;

import java.net.InetSocketAddress;
import java.util.List;
import java.util.concurrent.atomic.AtomicBoolean;

/** Integrates an app-owned Minestom server with Chunk's generic process lifecycle. */
public final class ChunkMinestom implements AutoCloseable {
    private final ChunkProcess chunk;
    private final ServerProcess server;
    private final TickExecutor ticks = new TickExecutor();
    private final ComponentRegistry components;
    private final ChunkSessions sessions;
    private final GameplayService gameplay;
    private final AtomicBoolean closed = new AtomicBoolean();
    private final Thread shutdownHook;
    private @Nullable Task task;
    private boolean started;

    private ChunkMinestom(ChunkProcess chunk, ServerProcess server) {
        this.chunk = chunk;
        this.server = server;
        var loader = Thread.currentThread().getContextClassLoader();
        var types = SessionRegistry.load(chunk.app(), loader);
        components = ComponentRegistry.load(loader);
        var manager =
                new SessionManager(
                        server,
                        ticks,
                        types,
                        components,
                        SessionMethodRegistry.load(chunk.app(), types.types(), loader));
        server.setCompressionThreshold(0);
        server.connectionManager().setPlayerProvider(ManagedPlayer::new);
        sessions = chunk.host(manager);
        gameplay = new GameplayService(manager, sessions);
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

    public synchronized void start() {
        if (started || closed.get())
            throw new IllegalStateException("Server already started or closed");
        try {
            task =
                    server.schedulerManager()
                            .buildTask(
                                    () -> {
                                        gameplay.flush();
                                        ticks.flush();
                                        chunk.tick();
                                    })
                            .repeat(TaskSchedule.tick(1))
                            .schedule();
            server.start(new InetSocketAddress(chunk.playerAddress(), 0));
            chunk.bind(server.server().getPort(), MinecraftConstants.PROTOCOL_VERSION);
            started = true;
        } catch (RuntimeException error) {
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
                        sessions::close,
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
