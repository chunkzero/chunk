package dev.chunkzero.backend.api;

public record SessionId(String value) {
    public SessionId {
        Codecs.platformId(value);
    }
}
