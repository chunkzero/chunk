# The environment image. Build from the repository root: `just image`.
ARG RUST_VERSION=1.98.1

FROM docker.io/library/rust:${RUST_VERSION}-trixie AS build
ARG RUST_VERSION
# rusqlite generates its SQLite bindings with libclang.
RUN apt-get update \
    && apt-get install -y --no-install-recommends libclang-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN grep -qx "channel = \"$RUST_VERSION\"" rust-toolchain.toml \
    || { echo "RUST_VERSION $RUST_VERSION does not match rust-toolchain.toml" >&2; exit 1; }
# V8's build script downloads its prebuilt static library into target/release/gn_out, which the target cache keeps.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p chunk-environment \
    && cp target/release/chunk-environment /usr/local/bin/ \
    && mkdir /data

FROM gcr.io/distroless/cc-debian13:nonroot
ARG VERSION
ARG REVISION
LABEL org.opencontainers.image.title="chunk-environment" \
      org.opencontainers.image.description="Environment process running the core, gateway and exec services" \
      org.opencontainers.image.source="https://github.com/chunkzero/chunk" \
      org.opencontainers.image.licenses="FSL-1.1-MIT" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"
COPY --from=build /usr/local/bin/chunk-environment /usr/local/bin/chunk-environment
COPY --from=build --chown=65532:65532 /data /data
ENV CHUNK_STATE=/data
VOLUME /data
# The gateway's player port. Backend and control bind loopback.
EXPOSE 25565
ENTRYPOINT ["/usr/local/bin/chunk-environment"]
