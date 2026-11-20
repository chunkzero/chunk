package com.chunkzero.chunk.runtime;

import org.jetbrains.annotations.Nullable;

import java.io.IOException;
import java.io.OutputStream;
import java.io.PrintStream;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
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
            // No autoflush: PrintStream would flush between the body and the newline of a println.
            super(new TaggingStream(target), false, StandardCharsets.UTF_8);
        }
    }

    /**
     * Holds each thread's output until its line is complete, then writes the line, tagged with the
     * session bound when it started, to the target in one write. All wrappers write under one lock,
     * so lines from different threads and streams never interleave. A line longer than {@link
     * #LINE_LIMIT} is written in pieces; only the first carries the tag. The target receives UTF-8
     * whatever its own charset. A thread's unflushed partial line is lost when the thread ends.
     */
    private static final class TaggingStream extends OutputStream {
        private static final int LINE_LIMIT = 8192;
        private static final Object LOCK = new Object();

        private final OutputStream target;
        private final ThreadLocal<Line> lines = ThreadLocal.withInitial(Line::new);

        TaggingStream(OutputStream target) {
            this.target = target;
        }

        @Override
        public void write(int b) throws IOException {
            write(new byte[] {(byte) b}, 0, 1);
        }

        @Override
        public void write(byte[] bytes, int offset, int length) throws IOException {
            var line = lines.get();
            var end = offset + length;
            while (offset < end) {
                if (line.isStart()) line.session = CURRENT.get();
                var newline = offset;
                while (newline < end && bytes[newline] != '\n') newline++;
                var stop = Math.min(newline + 1, end);
                line.append(bytes, offset, stop - offset);
                offset = stop;
                if (newline < end) emit(line, true);
                else if (line.size >= LINE_LIMIT) emit(line, false);
            }
        }

        @Override
        public void flush() throws IOException {
            var line = lines.get();
            if (line.size > 0) emit(line, false);
            synchronized (LOCK) {
                target.flush();
            }
        }

        private void emit(Line line, boolean complete) throws IOException {
            var session = line.continued ? null : line.session;
            var tag =
                    session == null
                            ? new byte[0]
                            : ("[session:" + session + "] ").getBytes(StandardCharsets.UTF_8);
            var output = new byte[tag.length + line.size];
            System.arraycopy(tag, 0, output, 0, tag.length);
            System.arraycopy(line.bytes, 0, output, tag.length, line.size);
            line.size = 0;
            line.continued = !complete;
            synchronized (LOCK) {
                target.write(output, 0, output.length);
                target.flush();
            }
        }
    }

    /** One thread's pending output on one stream. */
    private static final class Line {
        byte[] bytes = new byte[256];
        int size;
        @Nullable String session;
        boolean continued;

        boolean isStart() {
            return size == 0 && !continued;
        }

        void append(byte[] source, int offset, int length) {
            if (size + length > bytes.length) {
                bytes = Arrays.copyOf(bytes, Math.max(bytes.length * 2, size + length));
            }
            System.arraycopy(source, offset, bytes, size, length);
            size += length;
        }
    }
}
