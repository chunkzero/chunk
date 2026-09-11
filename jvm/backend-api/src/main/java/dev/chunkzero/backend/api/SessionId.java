package dev.chunkzero.backend.api;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonValue;

public record SessionId(@JsonValue String value) {
    @JsonCreator(mode = JsonCreator.Mode.DELEGATING)
    public SessionId {
        PlatformIds.check(value);
    }
}
