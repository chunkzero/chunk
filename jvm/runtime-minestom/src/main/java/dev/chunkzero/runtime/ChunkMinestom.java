package dev.chunkzero.runtime;

import chunk.v1.Supervision.SessionInventory;

import dev.chunkzero.runtime.minestom.internal.AppRegistry;
import dev.chunkzero.runtime.minestom.internal.GameplayService;
import dev.chunkzero.runtime.minestom.internal.ProcessService;

import net.minestom.server.MinecraftServer;
import net.minestom.server.timer.Task;
import net.minestom.server.timer.TaskSchedule;

import org.jetbrains.annotations.Nullable;

import java.io.IOException;
import java.util.List;
import java.util.concurrent.atomic.AtomicBoolean;

/** Integrates an app-owned Minestom server with Chunk's generic process lifecycle. */
public final class ChunkMinestom implements AutoCloseable {
    private final ChunkProcess process;
    private final MinecraftServer minecraft;
    private final TickExecutor ticks = new TickExecutor();
    private final SessionManager sessions;
    private final GameplayService gameplay;
    private final ProcessService service;
    private final AtomicBoolean closed = new AtomicBoolean();
    private final Thread shutdownHook;
    private @Nullable Task task;
    private boolean started;

    private ChunkMinestom(ChunkProcess process, MinecraftServer minecraft) {
        this.process = process;
        this.minecraft = minecraft;
        var factories =
                AppRegistry.load(
                        process.identity().getAppId(),
                        Thread.currentThread().getContextClassLoader());
        sessions =
                new SessionManager(ticks, factories, (session, appId) -> process.backend(session));
        MinecraftServer.setCompressionThreshold(0);
        MinecraftServer.getConnectionManager().setPlayerProvider(ManagedPlayer::new);
        var identity = process.identity();
        gameplay =
                new GameplayService(
                        identity.getDeployment(),
                        identity.getGeneration(),
                        sessions,
                        System::nanoTime,
                        identity.getRuntimeId(),
                        process::isReady);
        service = new ProcessService(identity, gameplay, sessions, process::tickCount, process);
        shutdownHook = new Thread(this::close, "chunk-minestom-shutdown");
        Runtime.getRuntime().addShutdownHook(shutdownHook);
    }

    /** Attach before starting the listener. Closing this integration stops Minestom. */
    public static ChunkMinestom attach(ChunkProcess process, MinecraftServer minecraft) {
        try {
            return new ChunkMinestom(process, minecraft);
        } catch (RuntimeException | Error error) {
            MinecraftServer.process().stop();
            throw error;
        }
    }

    public synchronized void start() throws IOException {
        if (started || closed.get())
            throw new IllegalStateException("Server already started or closed");
        try {
            task =
                    MinecraftServer.getSchedulerManager()
                            .buildTask(
                                    () -> {
                                        ticks.flush();
                                        gameplay.flush();
                                        var inventory = sessions.inventory();
                                        process.progress(
                                                sessions.activeCount(),
                                                inventory.stream()
                                                        .mapToInt(SessionInventory::getAttached)
                                                        .sum());
                                    })
                            .repeat(TaskSchedule.tick(1))
                            .schedule();
            minecraft.start("127.0.0.1", 0);
            gameplay.setEndpoint("127.0.0.1:" + MinecraftServer.process().server().getPort());
            process.bind(List.of(gameplay, service), gameplay.getEndpoint());
            started = true;
        } catch (IOException | RuntimeException error) {
            close();
            throw error;
        }
    }

    @Override
    public synchronized void close() {
        if (!closed.compareAndSet(false, true)) return;
        MinecraftServer.process().stop();
        if (task != null) task.cancel();
        gameplay.close();
        if (Thread.currentThread() != shutdownHook) {
            try {
                Runtime.getRuntime().removeShutdownHook(shutdownHook);
            } catch (IllegalStateException ignored) {
                /* JVM shutdown already started. */
            }
        }
    }
}
