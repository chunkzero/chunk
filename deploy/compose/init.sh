#!/bin/sh
# Writes the .env compose.yaml reads, with fresh secrets and the local engine socket. Never overwrites one.
set -eu

env_file=${1:-"$(dirname "$0")/.env"}
if [ -e "$env_file" ]; then
  echo "$env_file already exists; not overwriting it" >&2
  exit 1
fi

hex() { od -An -vtx1 -N"$1" /dev/urandom | tr -d ' \n'; }

rootless="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/podman/podman.sock"
if [ ! -S /var/run/docker.sock ] && [ -S "$rootless" ]; then
  socket=$rootless
  gid=0
else
  socket=/var/run/docker.sock
  gid=$(stat -c %g "$socket" 2>/dev/null || echo 0)
fi

umask 077
set -C
cat > "$env_file" <<ENV
CHUNK_SECRET_KEY=$(head -c 32 /dev/urandom | base64)
CHUNK_OPERATOR_TOKEN=chunk_$(hex 32)
CHUNK_EDGE_TOKEN=chunk_$(hex 32)
POSTGRES_PASSWORD=$(hex 24)
CHUNK_ENGINE_SOCKET=$socket
CHUNK_ENGINE_GID=$gid
ENV
chmod 600 "$env_file"
echo "wrote $env_file (engine socket $socket)"
