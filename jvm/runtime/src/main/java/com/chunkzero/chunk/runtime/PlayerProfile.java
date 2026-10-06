package com.chunkzero.chunk.runtime;

import org.jetbrains.annotations.Nullable;

import java.util.List;
import java.util.Objects;
import java.util.UUID;

/**
 * Who a delivered player authenticated as at the gateway. Engines admit the player with this
 * profile, including its signed properties.
 */
public record PlayerProfile(UUID uuid, String name, List<Property> properties) {
    public PlayerProfile {
        Objects.requireNonNull(uuid);
        Objects.requireNonNull(name);
        properties = List.copyOf(properties);
    }

    /** A profile property, such as {@code textures}. */
    public record Property(String name, String value, @Nullable String signature) {
        public Property {
            Objects.requireNonNull(name);
            Objects.requireNonNull(value);
        }
    }
}
