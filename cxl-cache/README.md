# File-backed CXL cache prototype

This prototype models one 24 GiB CXL 3.0 shared-memory pool with a sparse
regular file. The allocator daemon exposes exactly two 12 GiB regions, one per
Pageserver. It is intentionally limited to a two-Pageserver experiment.

## Local Docker workflow

Build and initialize the environment:

```bash
docker compose exec neon bash docker/local/neon-control.sh build
docker compose exec neon bash docker/local/neon-control.sh init
docker compose exec neon bash docker/local/neon-control.sh start
```

`init` creates a fresh Neon environment with one Pageserver and configures the
`main` Compute endpoint with:

```text
neon.protocol_version = 4
neon.cxl_cache_enabled = true
neon.cxl_cache_file = '/workspace/neon/.neon/cxl-cache/pool.bin'
```

The control script starts `cxl-cache-daemon` before the storage services. The
pool is stored at `.neon/cxl-cache/pool.bin`, has 24 GiB of logical data space,
and contains two 12 GiB regions. Because it is sparse, it consumes disk blocks
only as cache slots are written.

The regular local deployment uses one Pageserver and therefore allocates
Region 0. Region 1 remains available for the focused two-Pageserver experiment.
The control script never deletes or rewrites an existing Neon repository
automatically.

## Focused two-Pageserver test

The focused test does not need a running Neon cluster:

```bash
docker compose exec neon bash docker/local/neon-control.sh cxl-test
```

It starts the allocator daemon, assigns Region 0 and Region 1 to two independent
writer processes, reads both pages through independent read-only mappings, and
checks the 24 GiB/12 GiB layout.

## Request and fallback behavior

Compute enables `GETPAGE_FLAG_ALLOW_SHARED` only with Pagestream V4 and the CXL
GUCs enabled. Pageserver returns either ordinary page bytes or a shared
location. Compute validates the pool UUID and epoch, region epoch, slot bounds,
sequence value before and after the copy, and CRC32C.

If any validation fails, Compute discards that response and retries the same
page with shared delivery disabled. The retry therefore receives ordinary page
bytes through Pagestream and does not turn a cache race into a database I/O
error.
