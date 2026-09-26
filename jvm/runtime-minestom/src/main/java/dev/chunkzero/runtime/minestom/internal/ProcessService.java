package dev.chunkzero.runtime.minestom.internal;

import chunk.v1.Supervision.DesiredSessions;
import chunk.v1.Supervision.ProcessIdentity;
import chunk.v1.Supervision.ProcessReport;

import dev.chunkzero.runtime.ChunkProcess;
import dev.chunkzero.runtime.SessionManager;
import dev.chunkzero.runtime.control.ProcessState;

import org.jetbrains.annotations.ApiStatus;

/** Runs the sessions control asks for and reports the engine's sessions and deliveries. */
@ApiStatus.Internal
public final class ProcessService implements ProcessState {
    private final ProcessIdentity identity;
    private final GameplayService gameplay;
    private final SessionManager sessions;
    private final ChunkProcess process;

    public ProcessService(
            ProcessIdentity identity,
            GameplayService gameplay,
            SessionManager sessions,
            ChunkProcess process) {
        this.identity = identity;
        this.gameplay = gameplay;
        this.sessions = sessions;
        this.process = process;
    }

    @Override
    public ProcessReport inventory() {
        return gameplay.inventory();
    }

    /** Outcomes, including failures, surface in later reports. */
    @Override
    public void apply(DesiredSessions desired) {
        for (var command : desired.getCreateList()) {
            if (!command.getIdentity().equals(identity)) continue;
            if (process.isReady()) sessions.create(command);
            else sessions.reject(command);
        }
        for (var command : desired.getFinishList()) {
            if (command.getIdentity().equals(identity)) sessions.finish(command);
        }
        for (var session : desired.getForgetList()) sessions.forget(session.getId());
    }
}
