package dev.chunkzero.runtime.minestom.internal;

import chunk.sync.v1.Jvm.JvmReport;
import chunk.sync.v1.Jvm.JvmSession;
import chunk.sync.v1.Jvm.JvmSessionPhase;
import chunk.sync.v1.Jvm.JvmSessionStatus;
import chunk.v1.Common.SessionRef;
import chunk.v1.Supervision.SessionCommand;
import chunk.v1.Supervision.SessionInventory;
import chunk.v1.Supervision.SessionPhase;

import com.google.protobuf.ByteString;
import com.google.protobuf.InvalidProtocolBufferException;

import dev.chunkzero.runtime.SessionManager;
import dev.chunkzero.runtime.control.ProcessState;

import org.jetbrains.annotations.ApiStatus;

import java.util.HashSet;
import java.util.Map;
import java.util.Set;
import java.util.function.BooleanSupplier;

/**
 * Runs the sessions core's topic asks for, and reports the engine's sessions. Core numbers no
 * session generations, so each session runs as generation 1.
 */
@ApiStatus.Internal
public final class ProcessService implements ProcessState {
    private final SessionManager sessions;
    private final BooleanSupplier ready;
    // The sessions the latest snapshot lists, those this service asked to create, and to finish.
    private volatile Set<String> desired = Set.of();
    private final Set<String> created = new HashSet<>();
    private final Set<String> finished = new HashSet<>();

    public ProcessService(SessionManager sessions, BooleanSupplier ready) {
        this.sessions = sessions;
        this.ready = ready;
    }

    @Override
    public JvmReport inventory() {
        var report = JvmReport.newBuilder();
        var desired = this.desired;
        for (var session : sessions.inventory()) {
            var id = session.getSession().getId();
            if (!desired.contains(id) && terminal(session.getPhase())) sessions.forget(id);
            else report.addSessions(status(session));
        }
        return report.build();
    }

    /** Outcomes, including failures, surface in later reports. */
    @Override
    public synchronized void apply(Map<String, ByteString> entries) {
        var listed = new HashSet<String>();
        for (var entry : entries.entrySet()) {
            if (!entry.getKey().startsWith("session/")) continue;
            var id = entry.getKey().substring("session/".length());
            listed.add(id);
            JvmSession session;
            try {
                session = JvmSession.parseFrom(entry.getValue());
            } catch (InvalidProtocolBufferException error) {
                continue;
            }
            var command = command(id, session);
            if (created.add(id) && !session.getFinish()) {
                if (ready.getAsBoolean()) sessions.create(command);
                else sessions.reject(command);
            }
            if (session.getFinish() && finished.add(id)) sessions.finish(command);
        }
        for (var id : Set.copyOf(created)) {
            if (listed.contains(id)) continue;
            created.remove(id);
            if (finished.add(id)) sessions.finish(command(id, JvmSession.getDefaultInstance()));
        }
        finished.retainAll(created);
        desired = Set.copyOf(listed);
    }

    private static SessionCommand command(String id, JvmSession session) {
        return SessionCommand.newBuilder()
                .setOperationId(id)
                .setSession(SessionRef.newBuilder().setId(id))
                .setGeneration(1)
                .setSessionType(session.getSessionType())
                .setCapacity(session.getCapacity())
                .setConfigurationJson(session.getConfigurationJson())
                .build();
    }

    static JvmSessionStatus status(SessionInventory session) {
        return JvmSessionStatus.newBuilder()
                .setId(session.getSession().getId())
                .setSessionType(session.getSessionType())
                .setCapacity(session.getCapacity())
                .setPhase(phase(session.getPhase()))
                .setPrepared(session.getPrepared())
                .setAttached(session.getAttached())
                .build();
    }

    private static JvmSessionPhase phase(SessionPhase phase) {
        return switch (phase) {
            case SESSION_PHASE_STARTING -> JvmSessionPhase.JVM_SESSION_PHASE_STARTING;
            case SESSION_PHASE_READY -> JvmSessionPhase.JVM_SESSION_PHASE_READY;
            case SESSION_PHASE_ENDING -> JvmSessionPhase.JVM_SESSION_PHASE_ENDING;
            case SESSION_PHASE_ENDED -> JvmSessionPhase.JVM_SESSION_PHASE_ENDED;
            case SESSION_PHASE_FAILED -> JvmSessionPhase.JVM_SESSION_PHASE_FAILED;
            default -> JvmSessionPhase.JVM_SESSION_PHASE_UNSPECIFIED;
        };
    }

    private static boolean terminal(SessionPhase phase) {
        return phase == SessionPhase.SESSION_PHASE_ENDED
                || phase == SessionPhase.SESSION_PHASE_FAILED;
    }
}
