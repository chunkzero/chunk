package dev.chunkzero.runtime.bootstrap;

import com.fasterxml.jackson.annotation.JsonProperty;

import org.jetbrains.annotations.ApiStatus;

import java.util.Map;
import java.util.Objects;

@ApiStatus.Internal
public record AppManifest(
        Integer version,
        String id,
        @JsonProperty("main_class") String mainClass,
        Map<String, Factory> sessions) {
    public AppManifest {
        Objects.requireNonNull(version);
        Objects.requireNonNull(id);
        Objects.requireNonNull(mainClass);
        sessions = Map.copyOf(sessions);
        if (version != 2
                || !id.matches("[A-Za-z_][A-Za-z0-9_]{0,127}")
                || mainClass.isBlank()
                || sessions.isEmpty()
                || sessions.size() > 128
                || sessions.keySet().stream()
                        .anyMatch(key -> !key.matches("[A-Za-z_][A-Za-z0-9_]{0,127}")))
            throw new IllegalArgumentException("Invalid app manifest");
    }

    public record Factory(
            String provider,
            @JsonProperty("machine_profile") String machineProfile,
            Integer capacity) {
        public Factory {
            Objects.requireNonNull(provider);
            Objects.requireNonNull(machineProfile);
            Objects.requireNonNull(capacity);
            if (provider.isBlank()
                    || !machineProfile.matches("[A-Za-z0-9_-]{1,128}")
                    || capacity < 1
                    || capacity > 128)
                throw new IllegalArgumentException("Invalid session declaration");
        }
    }
}
