package dev.chunkzero.backend.client;

import java.util.UUID;

public record OperationId(String value) {
    public OperationId {
        if (value == null || value.isEmpty() || value.length() > 256) throw new IllegalArgumentException("Invalid operation identity");
    }
    public static OperationId create() { return new OperationId(UUID.randomUUID().toString()); }
}
