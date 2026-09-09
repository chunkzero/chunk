package dev.chunkzero.backend.api;

import com.google.gson.JsonElement;

public interface Codec<T> {
    T decode(JsonElement value);
    JsonElement encode(T value);
}
