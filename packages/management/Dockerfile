# The management image, serving the dashboard too. Build from the repository root: `just management-image`.
ARG NODE_VERSION=24.20.0
ARG BUN_VERSION=1.4.2

FROM docker.io/library/node:${NODE_VERSION}-trixie-slim AS build
ENV COREPACK_ENABLE_DOWNLOAD_PROMPT=0
WORKDIR /src
RUN corepack enable
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY packages/management/package.json packages/management/
COPY packages/dashboard/package.json packages/dashboard/
RUN pnpm install --frozen-lockfile --filter @chunkzero/management --filter @chunkzero/dashboard
COPY packages packages
COPY deny.toml ./
COPY scripts/npm-notices.mjs scripts/spdx.mjs scripts/
COPY licenses/npm licenses/npm
RUN pnpm build:dashboard \
    && pnpm --filter @chunkzero/management deploy --prod --legacy /out \
    && cp -r packages/dashboard/dist /out/dashboard \
    && node scripts/npm-notices.mjs @chunkzero/management > /out/THIRD_PARTY_LICENSES \
    && node scripts/npm-notices.mjs @chunkzero/dashboard > /out/dashboard/THIRD_PARTY_LICENSES.txt \
    && mkdir /data

FROM docker.io/oven/bun:${BUN_VERSION}-distroless
ARG VERSION
ARG REVISION
LABEL org.opencontainers.image.title="chunk-management" \
      org.opencontainers.image.description="Management service and dashboard for a self-hosted chunk install" \
      org.opencontainers.image.source="https://github.com/chunkzero/chunk" \
      org.opencontainers.image.licenses="FSL-1.1-MIT" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"
WORKDIR /app
COPY --from=build /out /app
COPY --from=build --chown=65532:65532 /data /data
ENV CHUNK_DATA_DIR=/data CHUNK_DASHBOARD_DIR=/app/dashboard
VOLUME /data
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/bun", "src/main.ts"]
