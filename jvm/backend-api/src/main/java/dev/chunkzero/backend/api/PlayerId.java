package dev.chunkzero.backend.api;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonValue;

public record PlayerId(@JsonValue String value) {
    @JsonCreator(mode = JsonCreator.Mode.DELEGATING)
    public PlayerId {
        PlatformIds.check(value);
    }
}
