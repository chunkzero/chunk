package dev.chunkzero.backend.client;

/**
 * An action may or may not have run, and core can no longer tell which. Calling it again runs it
 * again, so only do so when running it twice is safe, such as under an idempotency key the action
 * checks.
 */
public final class OutcomeUnknownException extends RuntimeException {
    private static final long serialVersionUID = 1L;

    OutcomeUnknownException(String message, Throwable cause) {
        super(message, cause);
    }
}
