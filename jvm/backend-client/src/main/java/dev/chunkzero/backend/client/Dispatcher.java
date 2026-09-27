package dev.chunkzero.backend.client;

import java.util.ArrayDeque;

/**
 * Runs a session's observer callbacks in order on one virtual thread, started on first use. A
 * subscription is queued once per pending state and delivers only its latest state.
 */
final class Dispatcher {
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

    synchronized boolean isCurrent() {
        return Thread.currentThread() == thread;
    }

    /**
     * Interrupts {@code subscription}'s callback in progress and waits for it to return, unless
     * called from that callback's thread.
     */
    synchronized void finish(GroupSubscription subscription) {
        if (running != subscription || isCurrent()) return;
        thread.interrupt();
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

    /** Stops later callbacks and interrupts one in progress. */
    synchronized void stop() {
        stopped = true;
        ready.clear();
        if (thread != null) thread.interrupt();
        notifyAll();
    }

    /** Waits for the thread to exit after {@link #stop}, unless called from it. */
    void join() {
        Thread current;
        synchronized (this) {
            current = thread;
        }
        if (current == null || current == Thread.currentThread()) return;
        boolean interrupted = false;
        while (true) {
            try {
                current.join();
                break;
            } catch (InterruptedException error) {
                interrupted = true;
            }
        }
        if (interrupted) Thread.currentThread().interrupt();
    }

    synchronized boolean finished() {
        return stopped && (thread == null || !thread.isAlive());
    }

    private void run() {
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
