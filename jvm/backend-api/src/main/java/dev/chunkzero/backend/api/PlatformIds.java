package dev.chunkzero.backend.api;

final class PlatformIds {
    private PlatformIds() {}

    static void check(String value) {
        if (value == null || !value.matches("[A-Za-z0-9_-]{1,128}"))
            throw new IllegalArgumentException("Invalid platform ID");
    }
}
