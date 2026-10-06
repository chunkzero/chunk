package com.chunkzero.chunk.runtime;

import java.util.function.BooleanSupplier;

/** One call of a session method, handed to {@link SessionHandler#method}. */
public final class SessionMethod {
    private final String name;
    private final String argumentsJson;
    private final Delivery delivery;
    private final BooleanSupplier start;

    SessionMethod(String name, String argumentsJson, Delivery delivery, BooleanSupplier start) {
        this.name = name;
        this.argumentsJson = argumentsJson;
        this.delivery = delivery;
        this.start = start;
    }

    /** The method's name. */
    public String name() {
        return name;
    }

    /** The method's arguments as a JSON document. */
    public String argumentsJson() {
        return argumentsJson;
    }

    /** The delivery of the player the call acts for. */
    public Delivery delivery() {
        return delivery;
    }

    /**
     * Authorizes the call right before its effects run. Returns true if the call may run: it is not
     * cancelled or past its deadline, and its session and delivery are still open. Otherwise the
     * call is cancelled and its effects must not run. A call whose handler completes normally
     * without ever starting it is cancelled; one whose handler fails without starting it fails. A
     * handler that queues the call before running it should first check, through {@link
     * #delivery()}, that the player is still connected, and call {@link Delivery#left()} if not.
     */
    public boolean start() {
        return start.getAsBoolean();
    }
}
