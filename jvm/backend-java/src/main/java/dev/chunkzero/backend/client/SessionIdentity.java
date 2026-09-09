package dev.chunkzero.backend.client;

import com.google.gson.JsonObject;
import dev.chunkzero.backend.api.PlayerId;
import dev.chunkzero.backend.api.SessionId;
import java.util.Objects;
import java.util.Optional;

/** Supplied by trusted session ownership, independently of function arguments. */
public record SessionIdentity(SessionId session, String app, Optional<PlayerId> player) {
    public SessionIdentity {
        Objects.requireNonNull(session); Objects.requireNonNull(player);
        if (app == null || !app.matches("[A-Za-z0-9_-]{1,128}")) throw new IllegalArgumentException("Invalid app identity");
    }
    JsonObject json() {
        var value = new JsonObject(); value.addProperty("session", session.value()); value.addProperty("app", app);
        player.ifPresent(id -> value.addProperty("player", id.value())); return value;
    }
}
