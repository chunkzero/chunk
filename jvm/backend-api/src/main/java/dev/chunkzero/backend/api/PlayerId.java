package dev.chunkzero.backend.api;

public record PlayerId(String value) {
    public PlayerId { Codecs.platformId(value); }
}
