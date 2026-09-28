# The remote JVM runner image. Build from the repository root: `just jvm-image`.
ARG RUST_VERSION=1.98.1
ARG JAVA_VERSION=25

FROM docker.io/library/rust:${RUST_VERSION}-trixie AS build
ARG RUST_VERSION
WORKDIR /src
COPY . .
RUN grep -qx "channel = \"$RUST_VERSION\"" rust-toolchain.toml \
    || { echo "RUST_VERSION $RUST_VERSION does not match rust-toolchain.toml" >&2; exit 1; }
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p chunk-jvm \
    && cp target/release/chunk-jvm /usr/local/bin/ \
    && mkdir -p /var/cache/chunk

FROM docker.io/library/eclipse-temurin:${JAVA_VERSION}-jre
ARG VERSION
ARG REVISION
LABEL org.opencontainers.image.title="chunk-jvm" \
      org.opencontainers.image.description="Runner that fetches a JVM's release from core, verifies it and supervises the JVM" \
      org.opencontainers.image.source="https://github.com/chunkzero/chunk" \
      org.opencontainers.image.licenses="FSL-1.1-MIT" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"
COPY --from=build /usr/local/bin/chunk-jvm /usr/local/bin/chunk-jvm
COPY --from=build --chown=65532:65532 /var/cache/chunk /var/cache/chunk
VOLUME /var/cache/chunk
USER 65532:65532
# The runner is PID 1: it forwards stop signals to the JVM and reaps orphaned processes.
ENTRYPOINT ["/usr/local/bin/chunk-jvm"]
