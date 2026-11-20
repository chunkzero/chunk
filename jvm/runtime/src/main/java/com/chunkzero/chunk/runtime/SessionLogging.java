package com.chunkzero.chunk.runtime;

import org.jetbrains.annotations.Nullable;

import java.io.IOException;
import java.io.OutputStream;
import java.io.PrintStream;
import java.nio.charset.StandardCharsets;
import java.util.function.Supplier;
import java.util.logging.ConsoleHandler;
import java.util.logging.Logger;

/**
 * Tags output with the session it runs for. While a thread has a session bound, each line it starts
 * on {@link System#out} or {@link System#err} begins with {@code [session:<id>] }; output from
 * other threads, such as startup and GC, stays untagged.
 */
public final class SessionLogging {
    private static final ThreadLocal<@Nullable String> CURRENT = new ThreadLocal<>();

    private SessionLogging() {}

    /** The session bound to the current thread, or {@code null}. */
    public static @Nullable String current() {
        return CURRENT.get();
    }

    /**
     * Binds {@code sessionId}, or nothing when {@code null}, to the current thread and returns the
     * previous binding, for code that restores it itself.
     */
    public static @Nullable String bind(@Nullable String sessionId) {
        var previous = CURRENT.get();
        if (sessionId == null) CURRENT.remove();
        else CURRENT.set(sessionId);
        return previous;
    }

    /** Runs {@code action} with {@code sessionId} bound to the current thread. */
    public static void run(String sessionId, Runnable action) {
        var previous = bind(sessionId);
        try {
            action.run();
        } finally {
            bind(previous);
        }
    }

    /** Calls {@code action} with {@code sessionId} bound to the current thread. */
    public static <T> T call(String sessionId, Supplier<T> action) {
        var previous = bind(sessionId);
        try {
            return action.get();
        } finally {
            bind(previous);
        }
    }

    /**
     * Replaces {@link System#out} and {@link System#err} with tagging streams, and points console
     * handlers {@code java.util.logging} already made at the new {@link System#err}. Logging
     * backends that look up the standard streams when they write, such as SLF4J's simple logger,
     * follow too.
     */
    public static synchronized void install() {
        if (System.out instanceof Tagging && System.err instanceof Tagging) return;
        System.setOut(wrap(System.out));
        System.setErr(wrap(System.err));
        var root = Logger.getLogger("");
        for (var handler : root.getHandlers()) {
            if (handler.getClass() != ConsoleHandler.class) continue;
            var replacement = new ConsoleHandler();
            replacement.setLevel(handler.getLevel());
            replacement.setFormatter(handler.getFormatter());
            replacement.setFilter(handler.getFilter());
            root.removeHandler(handler);
            root.addHandler(replacement);
        }
    }

    static PrintStream wrap(PrintStream target) {
        if (target instanceof Tagging) return target;
        return new Tagging(target);
    }

    private static final class Tagging extends PrintStream {
        Tagging(PrintStream target) {
            super(new TaggingStream(target), true, target.charset());
        }
    }

    /** Writes through to its target, inserting the bound session's tag at each line start. */
    private static final class TaggingStream extends OutputStream {
        private final OutputStream target;
        private boolean lineStart = true;

        TaggingStream(OutputStream target) {
            this.target = target;
        }

        @Override
        public synchronized void write(int b) throws IOException {
            write(new byte[] {(byte) b}, 0, 1);
        }

        @Override
        public synchronized void write(byte[] bytes, int offset, int length) throws IOException {
            var session = CURRENT.get();
            if (session == null) {
                target.write(bytes, offset, length);
                if (length > 0) lineStart = bytes[offset + length - 1] == '\n';
                return;
            }
            var tag = ("[session:" + session + "] ").getBytes(StandardCharsets.UTF_8);
            var end = offset + length;
            while (offset < end) {
                if (lineStart) target.write(tag);
                var newline = offset;
                while (newline < end && bytes[newline] != '\n') newline++;
                var stop = Math.min(newline + 1, end);
                target.write(bytes, offset, stop - offset);
                lineStart = newline < end;
                offset = stop;
            }
        }

        @Override
        public synchronized void flush() throws IOException {
            target.flush();
        }
    }
}
