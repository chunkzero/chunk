package dev.chunkzero.backend.client;

import dev.chunkzero.backend.api.BackendJson;
import dev.chunkzero.backend.api.PlayerId;
import dev.chunkzero.backend.api.SessionId;

import tools.jackson.databind.node.ObjectNode;

import java.util.Objects;
import java.util.Optional;

/** Supplied by trusted session ownership, independently of function arguments. */
public record SessionIdentity(SessionId session, String app, Optional<PlayerId> player) {
    public SessionIdentity {
        Objects.requireNonNull(session);
        Objects.requireNonNull(player);
        if (app == null || !app.matches("[A-Za-z0-9_-]{1,128}"))
            throw new IllegalArgumentException("Invalid app identity");
    }

    ObjectNode json() {
        var value = BackendJson.mapper().createObjectNode();
        value.put("session", session.value());
        value.put("app", app);
        player.ifPresent(id -> value.put("player", id.value()));
        return value;
    }
}
