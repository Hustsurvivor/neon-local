# Neon 文件模拟 CXL Cache 部署与使用说明

本文档说明如何在当前 Docker 单机开发环境中部署和使用文件模拟的
CXL 3.0 共享页面缓存。常规部署使用一个 Pageserver；24 GiB 数据池仍
划分为两个 12 GiB Region，其中单 Pageserver 使用 Region 0，Region 1
保留给双 Pageserver 专项测试。

该实现面向本地功能验证，不作为生产环境部署方案。

## 1. 已实现的链路

一次共享页面读取经过以下路径：

1. Compute 通过 Pagestream V4 发送带 `ALLOW_SHARED` 标志的 GetPage 请求。
2. Pageserver 重建页面或命中自身 CXL 索引。
3. Pageserver 把 8 KiB 页面发布到自己的 12 GiB Region。
4. Pageserver 只向 Compute 返回 pool、Region、slot、sequence、文件偏移和
   CRC32C，不通过网络返回完整页面。
5. Compute 通过只读 `mmap` 从共享文件复制页面。
6. Compute 校验 pool/Region epoch、slot 边界、复制前后 sequence 和 CRC32C。
7. 任一校验失败时，Compute 自动以 `flags=0` 重试，由 Pageserver 通过普通
   Pagestream 返回完整页面。

共享缓存文件是 sparse file。数据池大小为 24 GiB，文件另有 8 KiB 控制区，
因此逻辑文件大小为 `25,769,811,968` 字节。文件不会在创建时立即占用
24 GiB 物理磁盘空间，只会随页面写入逐渐增长。

## 2. 前置条件

- Windows 上已安装并启动 Docker Desktop。
- 使用 Docker Compose v2。
- 当前工作目录是仓库根目录，例如 `D:\project\neon`。
- Docker Desktop 建议至少提供 8 个 CPU 和 16 GiB 内存。
- 仓库当前位于 `cxl-cache` 分支。

检查 Docker：

```powershell
docker version
docker compose version
docker compose ps
```

## 3. 创建开发容器

在 Windows PowerShell 中执行：

```powershell
docker compose up -d --build
```

检查容器：

```powershell
docker compose ps
```

正常情况下 `neon-local-neon-1` 最终会显示为 `healthy`。

代码通过 bind mount 映射到：

```text
/workspace/neon
```

Neon 数据、CXL pool 和运行日志保存在 Docker 命名卷挂载的：

```text
/workspace/neon/.neon
```

## 4. 编译

执行完整构建：

```powershell
docker compose exec neon bash docker/local/neon-control.sh build
```

顶层 `make` 会构建多个 PostgreSQL 版本和 Neon Rust 二进制，首次构建可能
持续较长时间。不要因为一段时间没有新日志就直接终止；可以在另一个终端检查：

```powershell
docker compose exec neon bash -lc "pgrep -a make; pgrep -a rustc"
```

主要产物：

```text
/workspace/neon/target/debug/pageserver
/workspace/neon/target/debug/cxl-cache-daemon
/workspace/neon/pg_install/v17/lib/postgresql/neon.so
```

验证产物包含 CXL 功能：

```powershell
docker compose exec neon bash -lc "strings pg_install/v17/lib/postgresql/neon.so | grep neon.cxl_cache_enabled"
docker compose exec neon bash -lc "strings target/debug/pageserver | grep NEON_CXL_CACHE_POOL_FILE | head -1"
```

## 5. 初始化

首次部署执行：

```powershell
docker compose exec neon bash docker/local/neon-control.sh init
```

该命令会：

- 初始化一个单 Pageserver Neon 环境；
- 创建默认 tenant；
- 创建 `main` Compute endpoint；
- 为 endpoint 写入 Pagestream V4 和 CXL 配置；
- 初始化完成后停止服务。

如果 `.neon/config` 已存在，脚本不会删除或重新初始化现有数据。

## 6. 启动和停止

启动：

```powershell
docker compose exec neon bash docker/local/neon-control.sh start
```

启动顺序为：

1. 以 `neon` 用户启动 `cxl-cache-daemon`；
2. 创建 24 GiB sparse pool；
3. 启动 broker、Pageserver、Safekeeper 和 storage controller；
4. 启动 `main` Compute。

停止：

```powershell
docker compose exec neon bash docker/local/neon-control.sh stop
```

查看状态：

```powershell
docker compose exec neon bash docker/local/neon-control.sh status
docker compose ps
```

不要直接以 root 身份执行 `cargo neon endpoint start/stop`。需要单独管理
endpoint 时使用：

```powershell
docker compose exec neon gosu neon bash -lc "cd /workspace/neon && cargo neon endpoint stop main"
docker compose exec neon gosu neon bash -lc "cd /workspace/neon && cargo neon endpoint start main"
```

## 7. 自动写入的 Compute 配置

控制脚本会在 `.neon/endpoints/main/postgresql.conf` 中确保存在：

```text
neon.protocol_version = 4
neon.cxl_cache_enabled = true
neon.cxl_cache_file = '/workspace/neon/.neon/cxl-cache/pool.bin'
```

数据库启动后验证：

```powershell
docker compose exec neon psql -h 127.0.0.1 -p 55432 -U cloud_admin -d postgres -Atc "show neon.protocol_version; show neon.cxl_cache_enabled; show neon.cxl_cache_file;"
```

预期输出：

```text
4
on
/workspace/neon/.neon/cxl-cache/pool.bin
```

## 8. 验证 daemon、Region 和 pool

确认 daemon、Pageserver 和 Compute 都不是 root：

```powershell
docker compose exec neon bash -lc 'ps -o user,pid,cmd -p $(cat .neon/cxl-cache/daemon.pid),$(cat .neon/pageserver_1/pageserver.pid),$(head -1 .neon/endpoints/main/pgdata/postmaster.pid)'
```

检查 pool 逻辑大小和实际磁盘占用：

```powershell
docker compose exec neon stat -c %s /workspace/neon/.neon/cxl-cache/pool.bin
docker compose exec neon du -h /workspace/neon/.neon/cxl-cache/pool.bin
```

逻辑大小应为：

```text
25769811968
```

检查 Region 0 状态：

```powershell
docker compose exec neon od -An -tu4 -N4 -j4096 /workspace/neon/.neon/cxl-cache/pool.bin
```

输出 `1` 表示 Region 0 已分配。

## 9. 连接数据库

进入容器连接：

```powershell
docker compose exec neon bash docker/local/neon-control.sh psql
```

也可以直接执行 SQL：

```powershell
docker compose exec neon psql -h 127.0.0.1 -p 55432 -U cloud_admin -d postgres
```

宿主机连接串：

```text
postgresql://cloud_admin@127.0.0.1:55432/postgres
```

必须显式使用 TCP 地址和端口。直接在容器中运行不带 `-h` 的 `psql -p 55432`
会尝试连接 Unix socket，而当前 Compute 只监听 `127.0.0.1:55432`。

## 10. SQL 端到端验证

创建约 91 MiB 的测试表：

```powershell
docker compose exec neon psql -h 127.0.0.1 -p 55432 -U cloud_admin -d postgres -v ON_ERROR_STOP=1 -c "DROP TABLE IF EXISTS cxl_e2e; CREATE TABLE cxl_e2e(id bigint PRIMARY KEY, payload text NOT NULL); INSERT INTO cxl_e2e SELECT g, repeat(md5(g::text), 32) FROM generate_series(1,80000) g; ANALYZE cxl_e2e; CHECKPOINT; SELECT count(*), sum(id), sum(length(payload)), pg_size_pretty(pg_total_relation_size('cxl_e2e')) FROM cxl_e2e;"
```

预期校验值：

```text
count                 80000
sum(id)               3200040000
sum(length(payload))  81920000
relation size         约 91 MB
```

重启 Compute，清空 PostgreSQL shared buffers：

```powershell
docker compose exec neon gosu neon bash -lc "cd /workspace/neon && cargo neon endpoint stop main && cargo neon endpoint start main"
```

执行冷读：

```powershell
docker compose exec neon psql -h 127.0.0.1 -p 55432 -U cloud_admin -d postgres -c "EXPLAIN (ANALYZE, BUFFERS, TIMING OFF) SELECT count(*), sum(id), sum(length(payload)) FROM cxl_e2e;"
```

当前验证环境中一次扫描读取约 11,420 个数据页，结果保持为上述校验值。

## 11. 查看 CXL 命中

正常运行时 PostgreSQL 的 `log_min_messages` 保持 `warning`。需要取证时，在容器
内临时追加：

```powershell
docker compose exec neon gosu neon bash -lc "cd /workspace/neon && grep -q '^log_min_messages = debug1' .neon/endpoints/main/postgresql.conf || printf '%s\n' 'log_min_messages = debug1' >> .neon/endpoints/main/postgresql.conf; cargo neon endpoint stop main; cargo neon endpoint start main"
```

执行 SQL 冷读后统计命中：

```powershell
docker compose exec neon grep -F -c "CXL cache hit:" /workspace/neon/.neon/endpoints/main/compute.log
docker compose exec neon grep -F "CXL cache hit:" /workspace/neon/.neon/endpoints/main/compute.log | Select-Object -Last 10
```

日志示例：

```text
[NEON_SMGR] CXL cache hit: region=0 epoch=1 slot=11740 control=6
```

统计自动回退：

```powershell
docker compose exec neon grep -F -c "CXL cache read failed" /workspace/neon/.neon/endpoints/main/compute.log
```

验证结束后恢复普通日志级别：

```powershell
docker compose exec neon gosu neon bash -lc "cd /workspace/neon && sed -i '/^log_min_messages = debug1/d' .neon/endpoints/main/postgresql.conf; cargo neon endpoint stop main; cargo neon endpoint start main"
```

## 12. 双 Pageserver 专项测试

常规部署不需要两个 Pageserver。共享文件 crate 提供独立双进程测试：

```powershell
docker compose exec neon bash docker/local/neon-control.sh cxl-test
```

测试会验证：

- 两个 Pageserver 分别获得 Region 0 和 Region 1；
- 两个 Region 各为 12 GiB；
- 两个独立 reader 可以读回正确页面；
- slot 覆盖后旧 sequence 被拒绝；
- Pageserver 重启后 region epoch 增加，旧位置失效。

测试完成后会清理临时 pool 和 socket。

## 13. 常见问题

### 13.1 `psql` 报 Unix socket 不存在

错误形式：

```text
connection to server on socket "/var/run/postgresql/.s.PGSQL.55432" failed
```

使用 TCP：

```bash
psql -h 127.0.0.1 -p 55432 -U cloud_admin -d postgres
```

### 13.2 `pg_ctl cannot be run as root`

Compute 不能由 root 启停。使用：

```bash
gosu neon bash -lc "cd /workspace/neon && cargo neon endpoint start main"
```

或统一使用 `docker/local/neon-control.sh`。

### 13.3 Region 状态仍为 0

确认使用了最新控制脚本，并完整停止、启动服务：

```powershell
docker compose exec neon bash docker/local/neon-control.sh stop
docker compose exec neon bash docker/local/neon-control.sh start
```

控制脚本会导出：

```text
NEON_CXL_CACHE_POOL_FILE
NEON_CXL_CACHE_DAEMON_SOCKET
```

### 13.4 pool 逻辑大小很大但磁盘占用很小

这是 sparse file 的预期行为。`stat` 显示逻辑大小，`du` 显示实际分配的磁盘
块。随着 Pageserver 发布页面，`du` 的结果会增长。

### 13.5 构建长时间没有输出

完整构建包含 PostgreSQL v14–v17 和 LTO 链接，可能需要数分钟。检查实际进程：

```powershell
docker compose exec neon bash -lc "ps -eo pid,etime,pcpu,cmd | grep -E 'make|rustc|gcc|ld' | grep -v grep"
```

只要编译器或链接器仍在消耗 CPU，就应继续等待。

## 14. 当前验证结论

单 Pageserver SQL 端到端验证已通过：

- daemon、Pageserver、Compute 均以 `neon` 用户运行；
- Region 0 正确分配；
- 91 MiB 表在 Compute 重启后校验值不变；
- 两次全表扫描产生 22,853 次 CXL 命中增量；
- 验证期间共享读取回退次数为 0；
- SQL、Pagestream V4、Pageserver 发布和 Compute `mmap` 读取形成完整闭环。
