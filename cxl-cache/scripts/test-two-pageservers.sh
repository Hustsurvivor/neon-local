#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
crate_dir="$repo_dir/cxl-cache"
target_dir=${CARGO_TARGET_DIR:-"$crate_dir/target"}
test_root=${NEON_CXL_TEST_ROOT:-"$repo_dir/.neon"}

mkdir -p "$test_root"
test_dir=$(mktemp -d "$test_root/cxl-test-XXXXXX")
daemon_pid=

cleanup() {
    if [[ -n "$daemon_pid" ]]; then
        kill "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    rm -f \
        "$test_dir/pool.bin" \
        "$test_dir/daemon.sock" \
        "$test_dir/daemon.log" \
        "$test_dir/page0.location" \
        "$test_dir/page1.location" \
        "$test_dir/page0-restarted.location"
    rmdir "$test_dir" 2>/dev/null || true
}
trap cleanup EXIT

cd "$crate_dir"
cargo build --bins

"$target_dir/debug/cxl-cache-daemon" \
    --pool-file "$test_dir/pool.bin" \
    --socket "$test_dir/daemon.sock" \
    >"$test_dir/daemon.log" 2>&1 &
daemon_pid=$!

for _ in $(seq 1 100); do
    [[ -S "$test_dir/daemon.sock" ]] && break
    sleep 0.05
done
[[ -S "$test_dir/daemon.sock" ]]

"$target_dir/debug/cxl-cache-writer" \
    --pool-file "$test_dir/pool.bin" \
    --socket "$test_dir/daemon.sock" \
    --pageserver-id 100 \
    --nonce 1001 \
    --slot 0 \
    --byte 17 \
    --location-file "$test_dir/page0.location"

"$target_dir/debug/cxl-cache-writer" \
    --pool-file "$test_dir/pool.bin" \
    --socket "$test_dir/daemon.sock" \
    --pageserver-id 200 \
    --nonce 2001 \
    --slot 0 \
    --byte 34 \
    --location-file "$test_dir/page1.location"

"$target_dir/debug/cxl-cache-reader" \
    --pool-file "$test_dir/pool.bin" \
    --location-file "$test_dir/page0.location"

"$target_dir/debug/cxl-cache-reader" \
    --pool-file "$test_dir/pool.bin" \
    --location-file "$test_dir/page1.location"

"$target_dir/debug/cxl-cache-writer" \
    --pool-file "$test_dir/pool.bin" \
    --socket "$test_dir/daemon.sock" \
    --pageserver-id 100 \
    --nonce 1002 \
    --slot 0 \
    --byte 51 \
    --location-file "$test_dir/page0-restarted.location"

if "$target_dir/debug/cxl-cache-reader" \
    --pool-file "$test_dir/pool.bin" \
    --location-file "$test_dir/page0.location"; then
    echo "old Pageserver location unexpectedly survived region epoch change" >&2
    exit 1
fi

"$target_dir/debug/cxl-cache-reader" \
    --pool-file "$test_dir/pool.bin" \
    --location-file "$test_dir/page0-restarted.location"

stat -c 'FILE_SIZE=%s' "$test_dir/pool.bin"
du -h "$test_dir/pool.bin"
cat "$test_dir/daemon.log"
