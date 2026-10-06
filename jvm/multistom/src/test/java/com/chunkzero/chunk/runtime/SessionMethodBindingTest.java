package com.chunkzero.chunk.runtime;

import static org.junit.jupiter.api.Assertions.*;

import com.chunkzero.chunk.backend.api.BackendValues;
import com.chunkzero.chunk.backend.api.JsonType;
import com.chunkzero.chunk.backend.api.SessionMethodRef;

import org.junit.jupiter.api.Test;

import tools.jackson.core.type.TypeReference;

import java.util.concurrent.atomic.AtomicInteger;

class SessionMethodBindingTest {
    @Test
    void validatesArgumentsBeforeCallingGameplayAndResultsBeforeReturning() {
        var text = JsonType.of(new TypeReference<String>() {}, BackendValues::checkString);
        var number = JsonType.of(new TypeReference<Long>() {}, BackendValues::checkInteger);
        var ref = new SessionMethodRef<>("lobby", "default", "length", text, number);
        var calls = new AtomicInteger();
        var session = new Session() {};
        var binding =
                new SessionMethodBinding<>(
                        ref,
                        (target, args) -> {
                            assertSame(session, target);
                            calls.incrementAndGet();
                            return (long) args.length();
                        });
        assertEquals("5", binding.invoke(session, "\"hello\""));
        assertThrows(RuntimeException.class, () -> binding.invoke(session, "42"));
        assertEquals(1, calls.get());
        var invalid = new SessionMethodBinding<>(ref, (target, args) -> Long.MAX_VALUE);
        assertThrows(RuntimeException.class, () -> invalid.invoke(session, "\"hello\""));
    }
}
