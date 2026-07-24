#!/usr/bin/env bash
set -euo pipefail

if [[ "$(id -u)" == "0" ]]; then
  exec gosu neon bash "$0" "$@"
fi

cd /workspace/neon

storage_services=(storage_broker storage_controller pageserver safekeeper endpoint_storage)
cxl_dir=/workspace/neon/.neon/cxl-cache
cxl_pool=${NEON_CXL_CACHE_POOL_FILE:-"$cxl_dir/pool.bin"}
cxl_socket=${NEON_CXL_CACHE_DAEMON_SOCKET:-"$cxl_dir/daemon.sock"}
cxl_pidfile="$cxl_dir/daemon.pid"
cxl_log="$cxl_dir/daemon.log"
export NEON_CXL_CACHE_POOL_FILE="$cxl_pool"
export NEON_CXL_CACHE_DAEMON_SOCKET="$cxl_socket"

cxl_cache_running() {
  [[ -f "$cxl_pidfile" ]] &&
    kill -0 "$(cat "$cxl_pidfile")" 2>/dev/null
}

start_cxl_cache() {
  if cxl_cache_running; then
    return
  fi

  mkdir -p "$cxl_dir"
  rm -f "$cxl_pidfile" "$cxl_socket"
  if [[ ! -x target/debug/cxl-cache-daemon ]]; then
    echo "CXL cache daemon is not built; run neon-control.sh build first." >&2
    exit 1
  fi

  nohup target/debug/cxl-cache-daemon \
    --pool-file "$cxl_pool" \
    --socket "$cxl_socket" \
    --pool-size 24GiB \
    --region-size 12GiB \
    >>"$cxl_log" 2>&1 &
  echo "$!" >"$cxl_pidfile"

  for _ in $(seq 1 100); do
    [[ -S "$cxl_socket" ]] && return
    sleep 0.05
  done
  echo "CXL cache daemon did not create $cxl_socket" >&2
  tail -n 40 "$cxl_log" >&2
  exit 1
}

stop_cxl_cache() {
  if cxl_cache_running; then
    kill "$(cat "$cxl_pidfile")"
    wait "$(cat "$cxl_pidfile")" 2>/dev/null || true
  fi
  rm -f "$cxl_pidfile" "$cxl_socket"
}

configure_endpoint_cxl() {
  local conf=.neon/endpoints/main/postgresql.conf
  mkdir -p "$(dirname "$conf")"
  grep -q '^neon\.protocol_version' "$conf" 2>/dev/null ||
    printf '%s\n' \
      "neon.protocol_version = 4" \
      "neon.cxl_cache_enabled = true" \
      "neon.cxl_cache_file = '$cxl_pool'" >>"$conf"
}

storage_services_running() {
  local service
  for service in "${storage_services[@]}"; do
    if ! pgrep -u "$(id -u)" -f "(^|/)${service}( |$)" >/dev/null; then
      return 1
    fi
  done
}

ensure_storage_started() {
  start_cxl_cache
  if storage_services_running; then
    echo "Neon storage services are already running."
  else
    # Recover cleanly from a partial start or stale pid files.
    cargo neon stop || true
    cargo neon start
  fi
}

usage() {
  echo "Usage: neon-control.sh {toolchain|build|init|start|stop|status|cxl-test|psql|shell}"
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
      cargo neon init --num-pageservers 1 --force empty-dir-ok
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
    configure_endpoint_cxl

    cargo neon stop
    stop_cxl_cache
    ;;
  start)
    ensure_storage_started
    configure_endpoint_cxl
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
    stop_cxl_cache
    ;;
  status)
    cargo neon endpoint list
    if cxl_cache_running; then
      echo "CXL cache daemon is running (pid $(cat "$cxl_pidfile"))."
    else
      echo "CXL cache daemon is stopped."
    fi
    ;;
  cxl-test)
    exec bash cxl-cache/scripts/test-two-pageservers.sh
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
