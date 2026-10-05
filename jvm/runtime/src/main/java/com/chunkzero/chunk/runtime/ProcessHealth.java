package com.chunkzero.chunk.runtime;

import chunk.sync.v1.Jvm.JvmHealth;

import java.lang.management.ManagementFactory;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

/** Engine progress is recorded on ticks, independently of the thread that reports it. */
final class ProcessHealth {
    private final AtomicLong ticks;
    private volatile long lastTick;
    private volatile int sessions;
    private volatile int players;
    private final AtomicBoolean ready = new AtomicBoolean();
    private final AtomicBoolean draining = new AtomicBoolean();

    ProcessHealth(AtomicLong ticks) {
        this.ticks = ticks;
    }

    void tick(int sessions, int players) {
        this.sessions = sessions;
        this.players = players;
        lastTick = System.nanoTime();
        ticks.incrementAndGet();
    }

    void ready(boolean value) {
        ready.set(value);
    }

    void drain() {
        draining.set(true);
    }

    boolean acceptsWork() {
        return ready.get() && !draining.get();
    }

    JvmHealth snapshot() {
        var memory = ManagementFactory.getMemoryMXBean().getHeapMemoryUsage();
        long count = 0;
        long millis = 0;
        for (var collector : ManagementFactory.getGarbageCollectorMXBeans()) {
            count += Math.max(0, collector.getCollectionCount());
            millis += Math.max(0, collector.getCollectionTime());
        }
        var os = ManagementFactory.getOperatingSystemMXBean();
        var cpu =
                os instanceof com.sun.management.OperatingSystemMXBean metrics
                        ? Math.max(0, metrics.getProcessCpuLoad())
                        : 0;
        var last = lastTick;
        return JvmHealth.newBuilder()
                .setReady(acceptsWork())
                .setDraining(draining.get())
                .setTickCount(ticks.get())
                .setLastTickAgeMillis(
                        last == 0
                                ? Long.MAX_VALUE
                                : TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - last))
                .setHeapUsedBytes(memory.getUsed())
                .setHeapMaxBytes(Math.max(0, memory.getMax()))
                .setGcCount(count)
                .setGcTimeMillis(millis)
                .setProcessCpuLoad(cpu)
                .setSessions(sessions)
                .setPlayers(players)
                .build();
    }
}
