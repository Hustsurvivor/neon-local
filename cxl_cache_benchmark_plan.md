# Baseline / CXL 切换与指标统计方案

## 1. 两个版本的定义

主对比不切换 Git 分支，使用同一套编译产物，只切换协议和 CXL 开关：

- Baseline：Pagestream V3，CXL 关闭，等价走原版 Neon GetPage 数据路径。
- CXL：Pagestream V4，CXL 开启。

这样可以避免重新编译、代码版本差异和数据库数据不一致。

## 2. 切换方式

### 切换到 Baseline

```powershell
docker compose exec neon psql -h 127.0.0.1 -p 55432 -U cloud_admin -d postgres `
  -c "ALTER SYSTEM SET neon.protocol_version = '3'"

docker compose exec neon psql -h 127.0.0.1 -p 55432 -U cloud_admin -d postgres `
  -c "ALTER SYSTEM SET neon.cxl_cache_enabled = 'off'"

docker compose exec neon gosu neon bash -lc "cargo neon endpoint stop main && cargo neon endpoint start main"
```

确认配置：

```powershell
docker compose exec neon psql -h 127.0.0.1 -p 55432 -U cloud_admin -d postgres `
  -Atc "show neon.protocol_version; show neon.cxl_cache_enabled"
```

预期：

```text
3
off
```

### 切换到 CXL

```powershell
docker compose exec neon psql -h 127.0.0.1 -p 55432 -U cloud_admin -d postgres `
  -c "ALTER SYSTEM SET neon.protocol_version = '4'"

docker compose exec neon psql -h 127.0.0.1 -p 55432 -U cloud_admin -d postgres `
  -c "ALTER SYSTEM SET neon.cxl_cache_enabled = 'on'"

docker compose exec neon psql -h 127.0.0.1 -p 55432 -U cloud_admin -d postgres `
  -c "ALTER SYSTEM SET neon.cxl_cache_file = '/workspace/neon/.neon/cxl-cache/pool.bin'"

docker compose exec neon gosu neon bash -lc "cargo neon endpoint stop main && cargo neon endpoint start main"
```

确认配置：

```powershell
docker compose exec neon psql -h 127.0.0.1 -p 55432 -U cloud_admin -d postgres `
  -Atc "show neon.protocol_version; show neon.cxl_cache_enabled; show neon.cxl_cache_file"
```

预期：

```text
4
on
/workspace/neon/.neon/cxl-cache/pool.bin
```

只重启 Compute，不重启 Pageserver 和 CXL daemon。这样租户数据不变，并且可以保留
已经预热的 CXL Cache。

如需测试“CXL 冷启动”，执行完整重启：

```powershell
docker compose exec neon bash docker/local/neon-control.sh stop
docker compose exec neon bash docker/local/neon-control.sh start
```

完整重启会清空 CXL Cache，但不会清空数据库。

## 3. SQL 性能指标

推荐使用 `pgbench`，两个模式必须使用完全相同的参数和随机种子：

```powershell
docker compose exec neon pgbench `
  -h 127.0.0.1 -p 55432 -U cloud_admin -d postgres `
  -f /workspace/neon/your_workload.sql `
  -c 8 -j 8 -T 180 -P 10 -r `
  --random-seed=20260725
```

每轮保存以下结果：

- `tps`：核心吞吐指标；
- `latency average`：平均事务延迟；
- 每条 SQL 的平均延迟；
- failed transactions；
- 测试持续时间。

如果需要 p50/p95/p99，增加事务日志：

```powershell
docker compose exec neon pgbench `
  -h 127.0.0.1 -p 55432 -U cloud_admin -d postgres `
  -f /workspace/neon/your_workload.sql `
  -c 8 -j 8 -T 180 -r -l `
  --sampling-rate=0.1 `
  --random-seed=20260725
```

从 `pgbench_log.*` 的事务耗时列计算 p50、p95 和 p99。

每个模式至少运行 5 次，执行顺序交错：

```text
Baseline -> CXL
CXL -> Baseline
Baseline -> CXL
CXL -> Baseline
Baseline -> CXL
```

最终统计中位数，并计算：

```text
TPS 提升率 = (CXL TPS / Baseline TPS - 1) * 100%
延迟降低率 = (1 - CXL latency / Baseline latency) * 100%
```

## 4. Neon GetPage 指标

测试前后分别抓取一次 Pageserver 指标：

```powershell
docker compose exec neon bash -lc "curl -fsS http://127.0.0.1:9898/metrics" `
  > pageserver-before.txt
```

测试结束后：

```powershell
docker compose exec neon bash -lc "curl -fsS http://127.0.0.1:9898/metrics" `
  > pageserver-after.txt
```

重点统计：

```text
pageserver_smgr_query_started_global_count{smgr_query_type="get_page_at_lsn"}
pageserver_smgr_query_seconds_global_count{smgr_query_type="get_page_at_lsn"}
pageserver_smgr_query_seconds_global_sum{smgr_query_type="get_page_at_lsn"}
pageserver_smgr_query_seconds_global_bucket{smgr_query_type="get_page_at_lsn",...}
```

所有值都使用测试后减测试前：

```text
GetPage 数量 = after_count - before_count
GetPage 平均耗时 =
    (after_sum - before_sum) / (after_count - before_count)
```

直方图 bucket 同样取差值，再计算 p50/p95/p99。

## 5. CXL 专用指标

当前实现只有 `DEBUG1` 命中日志，不适合正式性能测试，因为逐页写日志会显著影响结果。
正式测试前补充无日志计数器：

Compute：

```text
cxl_cache_read_hits_total
cxl_cache_read_bytes_total
cxl_cache_fallbacks_total
cxl_cache_fallback_pool_total
cxl_cache_fallback_slot_total
cxl_cache_fallback_checksum_total
cxl_cache_copy_seconds_count
cxl_cache_copy_seconds_sum
cxl_cache_copy_seconds_bucket
cxl_cache_crc_seconds_count
cxl_cache_crc_seconds_sum
cxl_cache_crc_seconds_bucket
```

Pageserver：

```text
pageserver_cxl_cache_lookup_total{result="hit|miss"}
pageserver_cxl_cache_publish_total{result="success|failure"}
pageserver_cxl_cache_shared_responses_total
pageserver_cxl_cache_evictions_total
pageserver_cxl_cache_bytes_avoided_total
```

计数器同样采用测试前后快照差值，不能直接使用进程启动以来的累计值。

核心派生指标：

```text
Compute CXL 命中率 =
    hits / (hits + fallbacks)

Pageserver Cache 命中率 =
    lookup_hit / (lookup_hit + lookup_miss)

Fallback 率 =
    fallbacks / (hits + fallbacks)

Pagestream 减少的数据量 =
    shared_responses * 8192

平均 CXL 复制耗时 =
    copy_seconds_sum / copy_seconds_count

平均 CRC 耗时 =
    crc_seconds_sum / crc_seconds_count
```

健康的 CXL 热缓存测试应满足：

- `shared_responses_total > 0`
- `cxl_cache_read_hits_total > 0`
- `shared_responses = hits + fallbacks`
- fallback 率低于 `0.01%`

## 6. 每轮固定执行顺序

1. 切换到 Baseline 或 CXL。
2. 重启 Compute。
3. 用 `SHOW` 验证当前模式。
4. 执行一次不计时预热。
5. 保存 Compute 和 Pageserver 指标快照。
6. 执行正式 SQL/pgbench 测试。
7. 保存测试后指标。
8. 计算指标差值并保存 pgbench 输出。
9. 切换到另一个模式，使用完全相同的 SQL、并发度和随机种子重复。

默认保持 LFC 配置不变；当前环境的 LFC 为 0，因此测试结果主要反映 Baseline
Pagestream 与 CXL 共享页面路径的差异。
