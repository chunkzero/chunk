package dev.chunkzero.backend.client;

import java.util.List;
import java.util.Objects;
import java.util.Optional;

public record GroupState(boolean stale, Optional<Snapshot> snapshot, Optional<String> error) {
    public GroupState {
        Objects.requireNonNull(snapshot);
        Objects.requireNonNull(error);
    }

    public static final class Snapshot {
        private final long revision;
        private final List<BoundQuery<?>> queries;
        private final List<QueryResult<?>> results;

        Snapshot(long revision, List<BoundQuery<?>> queries, List<QueryResult<?>> results) {
            this.revision = revision;
            this.queries = List.copyOf(queries);
            this.results = List.copyOf(results);
            if (queries.size() != results.size())
                throw new IllegalArgumentException("Query group shape mismatch");
        }

        public long revision() {
            return revision;
        }

        List<QueryResult<?>> results() {
            return results;
        }

        @SuppressWarnings("unchecked")
        public <T> QueryResult<T> result(BoundQuery<T> query) {
            int index = queries.indexOf(query);
            if (index < 0)
                throw new IllegalArgumentException("Query does not belong to this group");
            // Only the transport constructs snapshots, using each slot's declared result type.
            return (QueryResult<T>) results.get(index);
        }
    }
}
