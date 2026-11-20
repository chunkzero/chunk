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
