package dev.chunkzero.backend.client;

import java.util.Objects;
import java.util.Optional;

public record WatchState<T>(boolean stale, Optional<Snapshot<T>> snapshot, Optional<String> error) {
    public WatchState { Objects.requireNonNull(snapshot); Objects.requireNonNull(error); }
    public record Snapshot<T>(long revision, QueryResult<T> result) {
        public Snapshot { Objects.requireNonNull(result); }
    }
}
