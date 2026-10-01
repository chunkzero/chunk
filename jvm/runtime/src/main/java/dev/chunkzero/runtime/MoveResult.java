package dev.chunkzero.runtime;

/** What core did with a request to move a player. */
public enum MoveResult {
    /**
     * Core queued the move, and the player's gateway carries it out. The destination's hooks may
     * still turn the player away.
     */
    ACCEPTED,
    /** The player holds no claim. */
    OFFLINE,
    /**
     * The player's delivery here is no longer their current claim, or they are still arriving or
     * already moving.
     */
    STALE,
    /** The destination admits one session, and that session is full. */
    FULL,
    /** The release offers no such destination. */
    UNKNOWN_DESTINATION
}
