#!/usr/bin/env bash
set -euo pipefail

if [[ "$(id -u)" == "0" ]]; then
  exec gosu neon bash "$0" "$@"
fi

cd /workspace/neon

storage_services=(storage_broker storage_controller pageserver safekeeper endpoint_storage)

storage_services_running() {
  local service
  for service in "${storage_services[@]}"; do
    if ! pgrep -u "$(id -u)" -f "(^|/)${service}( |$)" >/dev/null; then
      return 1
    fi
  done
}

ensure_storage_started() {
  if storage_services_running; then
    echo "Neon storage services are already running."
  else
    # Recover cleanly from a partial start or stale pid files.
    cargo neon stop || true
    cargo neon start
  fi
}

usage() {
  echo "Usage: neon-control.sh {toolchain|build|init|start|stop|status|psql|shell}"
}

case "${1:-}" in
  toolchain)
    rustc --version
    cargo --version
    protoc --version
    poetry --version
    psql --version
    ;;
  build)
    make -j"${NEON_BUILD_JOBS:-8}" -s
    ;;
  init)
    if [[ ! -f .neon/config ]]; then
      # Docker creates the named-volume mountpoint before this command runs.
      # Accept that directory only when it is otherwise empty.
      cargo neon init --force empty-dir-ok
    fi

    ensure_storage_started

    if cargo neon tenant list | grep -Eq '[0-9a-f]{32}'; then
      echo "At least one tenant already exists; leaving tenant data unchanged."
    else
      cargo neon tenant create --set-default
    fi

    if cargo neon endpoint list | grep -Eq '^[[:space:]]*main[[:space:]]'; then
      echo "Endpoint main already exists."
    else
      cargo neon endpoint create main
    fi

    cargo neon stop
    ;;
  start)
    ensure_storage_started
    if cargo neon endpoint list 2>/dev/null | grep -Eq '^[[:space:]]*main[[:space:]].*[[:space:]]running[[:space:]]*$'; then
      echo "Endpoint main is already running."
    else
      cargo neon endpoint start main
    fi
    ;;
  stop)
    if [[ -f .neon/config ]]; then
      cargo neon stop || true
    fi
    ;;
  status)
    cargo neon endpoint list
    ;;
  psql)
    shift
    exec psql "postgresql://cloud_admin@127.0.0.1:55432/postgres" "$@"
    ;;
  shell)
    exec bash
    ;;
  *)
    usage
    exit 2
    ;;
esac
