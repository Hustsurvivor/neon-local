#!/usr/bin/env bash
set -euo pipefail

workspace=/workspace/neon

mkdir -p \
  "$workspace/.neon" \
  "$workspace/target" \
  "$workspace/pg_install" \
  /home/neon/.cargo
chown neon:neon \
  "$workspace/.neon" \
  "$workspace/target" \
  "$workspace/pg_install" \
  /home/neon/.cargo

if [[ "${1:-}" != "keepalive" ]]; then
  exec gosu neon "$@"
fi

shutdown() {
  gosu neon bash -lc 'cd /workspace/neon && if [[ -f .neon/config ]]; then cargo neon stop || true; fi'
  exit 0
}

trap shutdown TERM INT
while true; do
  sleep 3600 &
  wait "$!"
done
