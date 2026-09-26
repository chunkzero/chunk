package dev.chunkzero.runtime.control;

import chunk.v1.Supervision.DesiredSessions;
import chunk.v1.Supervision.ProcessReport;

import org.jetbrains.annotations.ApiStatus;

/** The sessions and deliveries an engine adapter holds, which it keeps in sync with control. */
@ApiStatus.Internal
public interface ProcessState {
    /** Every session and delivery the process holds, without its identity. */
    ProcessReport inventory();

    /** Creates and ends sessions as control asks. */
    void apply(DesiredSessions desired);
}
