package com.chunkzero.chunk.runtime;

import static org.junit.jupiter.api.Assertions.*;

import com.chunkzero.chunk.backend.api.BackendValues;
import com.chunkzero.chunk.backend.api.JsonType;
import com.chunkzero.chunk.backend.api.SessionMethodRef;

import org.junit.jupiter.api.Test;

import tools.jackson.core.type.TypeReference;

import java.util.List;
import java.util.Set;
import java.util.concurrent.atomic.AtomicInteger;

class SessionMethodBindingTest {
    @Test
    void validatesArgumentsBeforeCallingGameplayAndResultsBeforeReturning() {
        var text = JsonType.of(new TypeReference<String>() {}, BackendValues::checkString);
        var number = JsonType.of(new TypeReference<Long>() {}, BackendValues::checkInteger);
        var ref = new SessionMethodRef<>("lobby", "default", "length", text, number);
        var calls = new AtomicInteger();
        var session = new Game();
        var binding =
                new SessionMethodBinding<>(
                        ref,
                        Game.class,
                        (target, args) -> {
                            assertSame(session, target);
                            calls.incrementAndGet();
                            return (long) args.length();
                        });
        assertEquals("5", binding.invoke(session, "\"hello\""));
        assertThrows(RuntimeException.class, () -> binding.invoke(session, "42"));
        assertEquals(1, calls.get());
        var invalid = new SessionMethodBinding<>(ref, Game.class, (target, args) -> Long.MAX_VALUE);
        assertThrows(RuntimeException.class, () -> invalid.invoke(session, "\"hello\""));
    }

    @Test
    void registryRejectsForeignBindingsAndSessionsOfAnotherClass() {
        var text = JsonType.of(new TypeReference<String>() {}, BackendValues::checkString);
        var binding =
                new SessionMethodBinding<>(
                        new SessionMethodRef<>("lobby", "default", "echo", text, text),
                        Game.class,
                        (target, args) -> args);
        for (var types : List.of(Set.of("arena/default"), Set.of("default"), Set.<String>of()))
            assertThrows(
                    IllegalArgumentException.class,
                    () -> new SessionMethodRegistry(types, List.of(binding)));
        var registry = new SessionMethodRegistry(Set.of("lobby/default"), List.of(binding));
        assertTrue(registry.declares("lobby/default", "echo"));
        assertFalse(registry.declares("lobby/default", "missing"));
        assertEquals("\"hi\"", registry.invoke("lobby/default", "echo", new Game(), "\"hi\""));
        assertThrows(
                ClassCastException.class,
                () -> registry.invoke("lobby/default", "echo", "not a game", "\"hi\""));
        assertThrows(
                IllegalArgumentException.class,
                () -> registry.invoke("lobby/default", "missing", new Game(), "\"hi\""));
    }

    private static final class Game {}
}
