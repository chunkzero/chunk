# The edge image. Build from the repository root: `just edge-image`.
ARG RUST_VERSION=1.98.1

FROM docker.io/library/rust:${RUST_VERSION}-trixie AS build
ARG RUST_VERSION
WORKDIR /src
COPY . .
RUN grep -qx "channel = \"$RUST_VERSION\"" rust-toolchain.toml \
    || { echo "RUST_VERSION $RUST_VERSION does not match rust-toolchain.toml" >&2; exit 1; }
# The target cache outlives any one source tree and cargo judges freshness by mtime, so the workspace's sources
# are touched to always rebuild its own crates.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    find crates proto -type f -exec touch {} + \
    && cargo build --release --locked -p chunk-edge \
    && cp target/release/chunk-edge /usr/local/bin/

FROM gcr.io/distroless/cc-debian13:nonroot
ARG VERSION
ARG REVISION
LABEL org.opencontainers.image.title="chunk-edge" \
      org.opencontainers.image.description="Edge that routes players' Minecraft connections by hostname to environment gateways" \
      org.opencontainers.image.source="https://github.com/chunkzero/chunk" \
      org.opencontainers.image.licenses="FSL-1.1-MIT" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"
COPY --from=build /usr/local/bin/chunk-edge /usr/local/bin/chunk-edge
# The player port.
EXPOSE 25565
ENTRYPOINT ["/usr/local/bin/chunk-edge"]
