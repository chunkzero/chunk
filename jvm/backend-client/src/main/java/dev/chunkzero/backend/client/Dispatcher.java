package dev.chunkzero.backend.client;

import java.util.ArrayDeque;

/**
 * Runs a session's observer callbacks in order on one virtual thread, started on first use. A
 * subscription is queued once per pending state and delivers only its latest state. The thread
 * exits only after its current callback returns.
 */
final class Dispatcher {
    private static final ThreadLocal<Boolean> DISPATCHING = ThreadLocal.withInitial(() -> false);
    private final ArrayDeque<GroupSubscription> ready = new ArrayDeque<>();
    private Thread thread;
    private GroupSubscription running;
    private boolean stopped;

    synchronized void schedule(GroupSubscription subscription) {
        if (stopped) return;
        ready.add(subscription);
        if (thread == null) thread = Thread.startVirtualThread(this::run);
        else notifyAll();
    }

    /** Whether this thread runs callbacks for any session. */
    static boolean dispatching() {
        return DISPATCHING.get();
    }

    /**
     * Interrupts {@code subscription}'s callback in progress, unless on its own thread, and if
     * {@code wait} waits for it to return. Waiting continues through interrupts, which it restores.
     */
    synchronized void finish(GroupSubscription subscription, boolean wait) {
        if (running != subscription || thread == Thread.currentThread()) return;
        thread.interrupt();
        if (!wait) return;
        boolean interrupted = false;
        while (running == subscription) {
            try {
                wait();
            } catch (InterruptedException error) {
                interrupted = true;
            }
        }
        if (interrupted) Thread.currentThread().interrupt();
    }

    /** Stops later callbacks and interrupts one in progress; returns the thread, if started. */
    synchronized Thread stop() {
        if (!stopped) {
            stopped = true;
            ready.clear();
            if (thread != null) thread.interrupt();
            notifyAll();
        }
        return thread;
    }

    synchronized boolean terminated() {
        return stopped && (thread == null || !thread.isAlive());
    }

    private void run() {
        DISPATCHING.set(true);
        while (true) {
            GroupSubscription next;
            synchronized (this) {
                while (ready.isEmpty() && !stopped) {
                    try {
                        wait();
                    } catch (InterruptedException ignored) {
                        // Only stop ends the loop.
                    }
                }
                if (stopped) return;
                next = running = ready.poll();
            }
            try {
                next.deliver();
            } finally {
                synchronized (this) {
                    running = null;
                    // An interrupt aimed at the finished callback must not reach the next one.
                    Thread.interrupted();
                    notifyAll();
                }
            }
        }
    }
}
