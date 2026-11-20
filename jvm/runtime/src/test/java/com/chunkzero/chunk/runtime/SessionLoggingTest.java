package com.chunkzero.chunk.runtime;

import static org.junit.jupiter.api.Assertions.*;

import chunk.sync.v1.Jvm.JvmSession;

import org.junit.jupiter.api.Test;

import java.io.ByteArrayOutputStream;
import java.io.PrintStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;
import java.util.concurrent.CountDownLatch;

class SessionLoggingTest {
    @Test
    void tagsOnlyLinesEmittedInsideASessionContext() {
        var raw = new ByteArrayOutputStream();
        var stream = SessionLogging.wrap(new PrintStream(raw, true, StandardCharsets.UTF_8));

        stream.println("startup message");
        SessionLogging.run(
                "sess-1",
                () -> {
                    stream.print("round ");
                    stream.println("started");
                    stream.println("line 1\nline 2");
                    stream.println();
                    SessionLogging.run("sess-2", () -> stream.println("nested"));
                    assertEquals("sess-1", SessionLogging.current());
                    stream.println("back to outer");
                });
        assertNull(SessionLogging.current());
        stream.println("gc message");

        var lines = raw.toString(StandardCharsets.UTF_8).lines().toList();
        assertEquals(
                List.of(
                        "startup message",
                        "[session:sess-1] round started",
                        "[session:sess-1] line 1",
                        "[session:sess-1] line 2",
                        "[session:sess-1] ",
                        "[session:sess-2] nested",
                        "[session:sess-1] back to outer",
                        "gc message"),
                lines);
    }

    @Test
    void concurrentLinesOnBothStreamsKeepTheirOwnTagAndBody() throws Exception {
        var raw = new ByteArrayOutputStream();
        var out = SessionLogging.wrap(new PrintStream(raw, true, StandardCharsets.UTF_8));
        var err = SessionLogging.wrap(new PrintStream(raw, true, StandardCharsets.UTF_8));
        var start = new CountDownLatch(1);
        var threads =
                List.of(writer("A", out, start), writer("B", err, start), writer("C", out, start));
        threads.forEach(Thread::start);
        start.countDown();
        for (var thread : threads) thread.join();

        var lines = raw.toString(StandardCharsets.UTF_8).lines().toList();
        assertEquals(3 * 2000, lines.size());
        for (var line : lines) {
            assertTrue(line.matches("\\[session:(.)\\] \\1 line \\d+"), line);
        }
    }

    @Test
    void encodesInUtf8WhateverTheTargetsCharset() {
        var raw = new ByteArrayOutputStream();
        var stream = SessionLogging.wrap(new PrintStream(raw, true, StandardCharsets.UTF_16));

        SessionLogging.run("s", () -> stream.println("héllo ✓"));
        stream.println("plain ✓");

        assertEquals("[session:s] héllo ✓\nplain ✓\n", raw.toString(StandardCharsets.UTF_8));
    }

    @Test
    void partialLinesAreWrittenWholeSoNoTagContinuesAnother() throws Exception {
        var raw = new ByteArrayOutputStream();
        var stream = SessionLogging.wrap(new PrintStream(raw, true, StandardCharsets.UTF_8));
        var firstPartial = new CountDownLatch(1);
        var other = new CountDownLatch(1);
        var first =
                new Thread(
                        () ->
                                SessionLogging.run(
                                        "A",
                                        () -> {
                                            stream.print("progress");
                                            stream.flush();
                                            firstPartial.countDown();
                                            try {
                                                other.await();
                                            } catch (InterruptedException interrupted) {
                                                return;
                                            }
                                            stream.println(" done");
                                        }));
        first.start();
        firstPartial.await();
        SessionLogging.run("B", () -> stream.println("message B"));
        other.countDown();
        first.join();

        assertEquals(
                List.of("[session:A] progress", "[session:B] message B", "[session:A]  done"),
                raw.toString(StandardCharsets.UTF_8).lines().toList());
    }

    @Test
    void aLineLongerThanTheLimitIsWrittenInBoundedTaggedPieces() {
        var raw = new ByteArrayOutputStream();
        var stream = SessionLogging.wrap(new PrintStream(raw, true, StandardCharsets.UTF_8));
        var big = "x".repeat(20_000).getBytes(StandardCharsets.UTF_8);

        SessionLogging.run(
                "s",
                () -> {
                    stream.write(big, 0, big.length);
                    stream.println();
                });

        var lines = raw.toString(StandardCharsets.UTF_8).lines().toList();
        assertEquals(
                List.of(8192, 8192, 3616), lines.stream().map(line -> line.length() - 12).toList());
        assertTrue(lines.stream().allMatch(line -> line.startsWith("[session:s] x")));
    }

    @Test
    void aLineSplitAtTheLimitKeepsMultibyteCharactersWhole() {
        var raw = new ByteArrayOutputStream();
        var stream = SessionLogging.wrap(new PrintStream(raw, true, StandardCharsets.UTF_8));

        SessionLogging.run("s", () -> stream.println("x".repeat(8191) + "✓"));

        assertEquals(
                List.of("[session:s] " + "x".repeat(8191), "[session:s] ✓"),
                raw.toString(StandardCharsets.UTF_8).lines().toList());
    }

    private static Thread writer(String session, PrintStream stream, CountDownLatch start) {
        return new Thread(
                () -> {
                    try {
                        start.await();
                    } catch (InterruptedException interrupted) {
                        return;
                    }
                    SessionLogging.run(
                            session,
                            () -> {
                                for (var i = 0; i < 2000; i++) {
                                    stream.println(session + " line " + i);
                                }
                            });
                });
    }

    @Test
    void chunkSessionsBindsSessionContextForHandlerCallbacks() {
        var observed = new ArrayList<String>();
        var handler =
                new SessionHandler() {
                    @Override
                    public void create(SessionControl session) {
                        observed.add("create:" + SessionLogging.current());
                        session.ready();
                    }

                    @Override
                    public void finish(SessionControl session) {
                        observed.add("finish:" + SessionLogging.current());
                        session.ended();
                    }

                    @Override
                    public CompletionStage<String> method(
                            SessionControl session, SessionMethod call) {
                        observed.add("method:" + SessionLogging.current());
                        return CompletableFuture.completedFuture("null");
                    }
                };
        var host = ChunkSessions.detached(handler, ignored -> null);
        var spec = JvmSession.newBuilder().setSessionType("app/default").setCapacity(4).build();
        host.create("abc-123", spec).join();
        host.finish("abc-123", spec).join();

        assertNull(SessionLogging.current());
        assertEquals(List.of("create:abc-123", "finish:abc-123"), observed);
    }
}
