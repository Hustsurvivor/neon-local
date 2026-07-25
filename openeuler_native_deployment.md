# Neon CXL Cache 在 openEuler 24.03 上的原生部署说明

本文档说明如何把当前 `cxl-cache` 分支部署到 QEMU 中的 openEuler 24.03
x86_64 虚拟机，并直接在 openEuler 上原生编译和运行 Neon。

本方案不使用 Docker，继续使用普通 sparse file 和 `mmap(MAP_SHARED)` 模拟
CXL 共享内存。所有 Neon 业务进程均以非 root 的 `neon` 用户运行。

## 1. 部署边界

本次部署包括：

- 单个 Pageserver；
- 单个 Safekeeper；
- 单个 Compute endpoint；
- Storage Broker、Storage Controller 和 Endpoint Storage；
- 文件后端 `cxl-cache-daemon`；
- 24 GiB CXL Pool，划分为两个 12 GiB Region；
- PostgreSQL 监听 `127.0.0.1:55432`。

本次部署不包括：

- Docker 或 Podman；
- QEMU CXL Type-3 设备；
- `/dev/dax`；
- 多节点或高可用部署；
- 公网直接暴露 PostgreSQL；
- 生产级备份和故障恢复。

当前实现仍是 `FileBackend`。未来使用 `/dev/dax` 时需要增加 `DaxBackend`，
不能只把 Pool 路径从普通文件改成设备文件。

## 2. 虚拟机资源

推荐的 QEMU Guest 配置：

```text
CPU       8 vCPU
内存      32 GiB，最低 16 GiB
磁盘      200 GiB，最低 120 GiB
架构      x86_64
系统      openEuler 24.03
文件系统  ext4 或 XFS
```

24 GiB Pool 是 sparse file，创建时不会立即占用 24 GiB 物理空间，但页面发布后
实际占用会逐渐增长。源码编译、PostgreSQL 安装目录、Neon 数据和测试数据也需要
额外磁盘空间。

建议将 `/workspace` 放在虚拟机本地 ext4/XFS 磁盘，不要使用 NFS、SMB 或
VirtioFS 共享源码目录作为 CXL Pool 所在文件系统。

## 3. 系统预检查

登录 openEuler 虚拟机：

```bash
cat /etc/openEuler-release
uname -m
free -h
df -h
getenforce
timedatectl
```

架构应为：

```text
x86_64
```

测试文件系统是否支持 sparse file：

```bash
mkdir -p /tmp/neon-sparse-test
truncate -s 24G /tmp/neon-sparse-test/pool.bin
stat -c '%s' /tmp/neon-sparse-test/pool.bin
du -h /tmp/neon-sparse-test/pool.bin
rm -f /tmp/neon-sparse-test/pool.bin
rmdir /tmp/neon-sparse-test
```

`stat` 应显示 `25769803776` 字节，而 `du` 应接近零。

## 4. 创建非 root 用户

创建专用用户和工作目录：

```bash
sudo useradd --create-home --shell /bin/bash neon
sudo mkdir -p /workspace
sudo chown neon:neon /workspace
```

增加文件描述符限制：

```bash
sudo tee /etc/security/limits.d/neon.conf >/dev/null <<'EOF'
neon soft nofile 65536
neon hard nofile 65536
EOF
```

后续编译、初始化、启动和停止操作都在 `neon` 用户下执行：

```bash
sudo -iu neon
```

不要用 root 执行以下命令：

```text
cargo neon init
cargo neon start
cargo neon endpoint start
cxl-cache-daemon
```

当前控制脚本在 root 下会尝试使用 Ubuntu 容器内的 `gosu`。openEuler 原生环境
不依赖 `gosu`，而是直接以 `neon` 用户运行脚本。

## 5. 安装编译依赖

以下系统依赖安装命令使用 root 或 sudo 执行。

安装基础开发工具：

```bash
sudo dnf groupinstall -y "Development Tools"
```

安装 Neon 依赖：

```bash
sudo dnf install -y \
  git curl wget unzip tar perl \
  gcc gcc-c++ make clang cmake libtool \
  flex bison pkgconf-pkg-config \
  readline-devel zlib-devel \
  openssl openssl-devel \
  libseccomp-devel \
  libcurl-devel \
  libicu-devel \
  libffi-devel \
  protobuf protobuf-compiler protobuf-devel \
  postgresql postgresql-contrib libpq-devel \
  python3 python3-devel python3-pip \
  lsof procps-ng
```

如果仓库中没有 `libpq-devel`，安装：

```bash
sudo dnf install -y postgresql-devel
```

仓库自身的 dnf 依赖清单参见 [README.md](README.md)。

检查 protobuf：

```bash
protoc --version
```

Neon 要求 `protoc` 不低于 3.15。如果系统版本过旧，固定安装 25.1：

```bash
cd /tmp
curl -LO https://github.com/protocolbuffers/protobuf/releases/download/v25.1/protoc-25.1-linux-x86_64.zip
sudo mkdir -p /opt/protobuf-25.1
sudo unzip protoc-25.1-linux-x86_64.zip -d /opt/protobuf-25.1
```

## 6. 安装 Rust 和 Poetry

切换到 `neon` 用户：

```bash
sudo -iu neon
```

安装仓库固定的 Rust 1.88.0：

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
  sh -s -- -y --profile default --default-toolchain 1.88.0

source "$HOME/.cargo/env"
rustup component add llvm-tools rustfmt clippy
```

安装 Poetry 1.8.5：

```bash
curl -sSL https://install.python-poetry.org |
  python3 - --version 1.8.5
```

将以下内容加入 `/home/neon/.bash_profile`：

```bash
export PATH="/opt/protobuf-25.1/bin:$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
export CARGO_HOME="$HOME/.cargo"
export RUSTUP_HOME="$HOME/.rustup"
export CARGO_TARGET_DIR="/workspace/neon/target"
export NEON_BUILD_JOBS=8
export RUST_BACKTRACE=1
export NEON_CXL_CACHE_POOL_FILE="/workspace/neon/.neon/cxl-cache/pool.bin"
export NEON_CXL_CACHE_DAEMON_SOCKET="/workspace/neon/.neon/cxl-cache/daemon.sock"
```

重新加载环境：

```bash
source ~/.bash_profile
```

验证：

```bash
rustc --version
cargo --version
protoc --version
poetry --version
psql --version
clang --version
cmake --version
```

## 7. 拉取源码

四个 PostgreSQL 子模块固定从 Neon 官方仓库
`https://github.com/neondatabase/postgres.git` 拉取。主仓库位于个人 GitHub
命名空间不会改变子模块来源，也不需要创建 `Hustsurvivor/postgres` 仓库。

使用 SSH 拉取当前远程分支：

```bash
cd /workspace
git clone \
  --depth 1 \
  --single-branch \
  --branch cxl-cache \
  --recurse-submodules \
  --shallow-submodules \
  --jobs 1 \
  git@github.com:Hustsurvivor/neon-local.git neon

cd /workspace/neon
git submodule sync --recursive
git submodule update --init --recursive --depth 1 --jobs 1
```

记录实际部署的提交，后续升级和回退都使用明确 SHA：

```bash
git rev-parse HEAD
git status --short
```

服务器没有 GitHub SSH key 时，可以使用 HTTPS：

```bash
cd /workspace
git clone \
  --depth 1 \
  --single-branch \
  --branch cxl-cache \
  --recurse-submodules \
  --shallow-submodules \
  --jobs 1 \
  https://github.com/Hustsurvivor/neon-local.git neon
```

浅拉取只省略 Git 历史。四个固定版本当前提交所对应的源码仍会下载，因为
`postgres_ffi` 编译时需要 PostgreSQL 14、15、16 和 17 的头文件。

已有仓库更新到浅拉取配置：

```bash
cd /workspace/neon
git pull --ff-only
git submodule sync --recursive
git submodule update \
  --init \
  --recursive \
  --depth 1 \
  --jobs 1
```

之前失败留下的部分下载可以继续复用，不需要先删除子模块目录。

如果 GitHub 拒绝以 depth 1 直接取得某个固定提交，只对失败版本执行精确拉取：

```bash
git -C vendor/postgres-v14 fetch --depth 1 origin \
  2155cb165d05f617eb2c8ad7e43367189b627703

git -C vendor/postgres-v15 fetch --depth 1 origin \
  2aaab3bb4a13557aae05bb2ae0ef0a132d0c4f85

git -C vendor/postgres-v16 fetch --depth 1 origin \
  a42351fcd41ea01edede1daed65f651e838988fc

git -C vendor/postgres-v17 fetch --depth 1 origin \
  1e01fcea2a6b38180021aa83e0051d95286d9096

git submodule update --init --recursive --jobs 1
```

验证 URL、浅拉取配置和子模块状态：

```bash
git config --file .gitmodules \
  --get-regexp '^submodule\..*\.(url|shallow)$'

for pg in 14 15 16 17; do
  git -C "vendor/postgres-v${pg}" \
    rev-parse --is-shallow-repository
done

git submodule status
```

四个 `rev-parse` 命令都应输出 `true`，`git submodule status` 不应出现以 `-`
或 `+` 开头的条目。不要使用 `--remote` 或在子模块内执行 `git pull`，否则可能
偏离主仓库固定的提交。

首次部署不从 Docker 命名卷复制 `.neon`，而是在 openEuler 上重新初始化环境。

## 8. 原生编译

现有 [docker/local/neon-control.sh](docker/local/neon-control.sh) 虽然位于
`docker/local`，但内部工作目录固定为 `/workspace/neon`，因此可以由 `neon`
用户在 openEuler 上直接使用。

检查工具链：

```bash
cd /workspace/neon
bash docker/local/neon-control.sh toolchain
```

执行默认 debug 构建：

```bash
bash docker/local/neon-control.sh build
```

首次构建时间可能较长。可以在另一个 SSH 会话中检查：

```bash
pgrep -a make
pgrep -a cargo
pgrep -a rustc
du -sh target pg_install
```

验证产物：

```bash
test -x target/debug/pageserver
test -x target/debug/cxl-cache-daemon
test -f pg_install/v17/lib/postgresql/neon.so

strings pg_install/v17/lib/postgresql/neon.so |
  grep neon.cxl_cache_enabled

strings target/debug/pageserver |
  grep NEON_CXL_CACHE_POOL_FILE |
  head -1
```

当前控制脚本明确启动 `target/debug/cxl-cache-daemon`，因此本部署使用 debug
构建。正式进行性能测试前，应单独让控制脚本支持 release 产物，不能只执行
`BUILD_TYPE=release make` 后继续使用现有启动路径。

## 9. 初始化 Neon

确认当前用户：

```bash
id
```

必须显示 `neon` 用户，然后执行：

```bash
cd /workspace/neon
bash docker/local/neon-control.sh init
```

该命令会：

1. 初始化单 Pageserver 环境；
2. 创建默认 tenant；
3. 创建 `main` Compute endpoint；
4. 写入 Pagestream V4 和 CXL 配置；
5. 初始化完成后停止 Neon 服务。

检查：

```bash
test -f .neon/config
grep -E 'neon.protocol_version|neon.cxl_cache' \
  .neon/endpoints/main/postgresql.conf
```

重复执行 `init` 不会删除已经存在的 `.neon` 数据。

## 10. 启动和停止

启动：

```bash
cd /workspace/neon
bash docker/local/neon-control.sh start
```

查看状态：

```bash
bash docker/local/neon-control.sh status
```

优雅停止：

```bash
bash docker/local/neon-control.sh stop
```

不要直接杀死 Compute、Pageserver 或 CXL daemon。优雅停止可以避免残留 PID 文件
和未完成的 PostgreSQL 关闭过程。

## 11. 首次验证

验证 daemon、Pageserver 和 Compute 都属于 `neon`：

```bash
ps -o user,pid,cmd \
  -p "$(cat .neon/cxl-cache/daemon.pid)" \
  -p "$(cat .neon/pageserver_1/pageserver.pid)" \
  -p "$(head -1 .neon/endpoints/main/pgdata/postmaster.pid)"
```

连接数据库：

```bash
psql -h 127.0.0.1 -p 55432 \
  -U cloud_admin -d postgres \
  -c 'select current_user, current_database(), version();'
```

验证 CXL 配置：

```bash
psql -h 127.0.0.1 -p 55432 \
  -U cloud_admin -d postgres \
  -Atc 'show neon.protocol_version;
        show neon.cxl_cache_enabled;
        show neon.cxl_cache_file;'
```

预期：

```text
4
on
/workspace/neon/.neon/cxl-cache/pool.bin
```

检查 Pool：

```bash
stat -c '%s' .neon/cxl-cache/pool.bin
du -h .neon/cxl-cache/pool.bin
od -An -tu4 -N4 -j4096 .neon/cxl-cache/pool.bin
```

Pool 逻辑大小应为：

```text
25769811968
```

该大小由 24 GiB 数据池和 8 KiB 控制区组成。Region 0 的状态应为 `1`，表示已经
分配给单 Pageserver。

## 12. 从外部连接

保持 PostgreSQL 只监听虚拟机的 `127.0.0.1:55432`。不要直接在防火墙中开放
55432。

从客户端建立 SSH 隧道：

```bash
ssh -L 55432:127.0.0.1:55432 <server-user>@<server-address>
```

然后在客户端连接：

```bash
psql -h 127.0.0.1 -p 55432 \
  -U cloud_admin -d postgres
```

如果 QEMU 使用 NAT，只需为 SSH 配置端口转发，不需要额外转发 PostgreSQL 端口。

## 13. 配置 systemd

只有手工初始化、启动、停止和 SQL 验证全部通过后，才配置自动启动。

以 root 创建 `/etc/systemd/system/neon-local.service`：

```ini
[Unit]
Description=Neon local CXL experiment
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
User=neon
Group=neon
WorkingDirectory=/workspace/neon
Environment=HOME=/home/neon
Environment=CARGO_HOME=/home/neon/.cargo
Environment=RUSTUP_HOME=/home/neon/.rustup
Environment=CARGO_TARGET_DIR=/workspace/neon/target
Environment=NEON_BUILD_JOBS=8
Environment=RUST_BACKTRACE=1
Environment=NEON_CXL_CACHE_POOL_FILE=/workspace/neon/.neon/cxl-cache/pool.bin
Environment=NEON_CXL_CACHE_DAEMON_SOCKET=/workspace/neon/.neon/cxl-cache/daemon.sock
Environment=PATH=/opt/protobuf-25.1/bin:/home/neon/.local/bin:/home/neon/.cargo/bin:/usr/local/bin:/usr/bin:/bin
ExecStart=/usr/bin/bash /workspace/neon/docker/local/neon-control.sh start
ExecStop=/usr/bin/bash /workspace/neon/docker/local/neon-control.sh stop
TimeoutStartSec=300
TimeoutStopSec=180
LimitNOFILE=65536
KillMode=control-group

[Install]
WantedBy=multi-user.target
```

加载并启用：

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now neon-local
sudo systemctl status neon-local
```

查看日志：

```bash
journalctl -u neon-local
tail -f /workspace/neon/.neon/cxl-cache/daemon.log
tail -f /workspace/neon/.neon/pageserver_1/pageserver.log
```

## 14. 验收

部署必须满足：

- openEuler 24.03 x86_64 环境检查通过；
- Rust、Poetry、protoc、clang、cmake 和 psql 可用；
- `make` 完整构建成功；
- Pageserver、Safekeeper、Storage Controller、Compute 和 CXL daemon 正常运行；
- 所有 Neon 业务进程属于 `neon` 用户；
- SQL 建表、插入和查询成功；
- Pool 逻辑大小为 `25769811968` 字节，并保持 sparse 特征；
- Region 0 已分配；
- Compute 使用 Pagestream V4 且启用 CXL；
- CXL 读取异常时能回退普通 Pagestream；
- 停止并重新启动后数据库内容仍然存在；
- CXL Cache 重启后可以重新填充。

最小 SQL 验证：

```bash
psql -h 127.0.0.1 -p 55432 \
  -U cloud_admin -d postgres \
  -v ON_ERROR_STOP=1 \
  -c "CREATE TABLE IF NOT EXISTS openeuler_deploy_check(
        id bigint PRIMARY KEY,
        value text NOT NULL
      );
      INSERT INTO openeuler_deploy_check
      VALUES (1, 'ok')
      ON CONFLICT (id) DO UPDATE SET value = EXCLUDED.value;
      SELECT * FROM openeuler_deploy_check;"
```

停止并启动后再次查询，结果必须仍为：

```text
1 | ok
```

## 15. 备份、升级和恢复

升级前先停止服务：

```bash
sudo systemctl stop neon-local
```

备份 `.neon`，但排除可重新生成的 CXL Cache：

```bash
sudo mkdir -p /var/backups
sudo tar -C /workspace/neon -czf \
  "/var/backups/neon-data-$(date +%F).tar.gz" \
  --exclude='.neon/cxl-cache' \
  .neon
```

不要把 `pool.bin` 纳入常规备份。它是易失缓存，读取整个 24 GiB sparse file
只会延长备份时间，不提供数据库恢复能力。

升级步骤：

1. 记录当前 Git SHA；
2. 停止 Neon；
3. 备份 `.neon`；
4. 拉取并 checkout 新的明确 SHA；
5. 更新全部子模块；
6. 重新编译；
7. 启动并完成 SQL 和 CXL 验证；
8. 验证失败时回退原 Git SHA 和备份。

禁止在没有备份的情况下：

- 删除 `/workspace/neon/.neon`；
- 再次执行破坏性初始化；
- 删除 Pool 之外的 Neon 数据；
- 覆盖未知版本的 `pg_install`；
- 使用 root 启动 Compute 或 Pageserver。

## 16. 后续迁移到 QEMU CXL/DAX

本次普通文件模拟不依赖 QEMU CXL 设备和 guest CXL 内核驱动。

后续迁移需要独立完成：

1. 在 QEMU 中配置 CXL host bridge、root port 和 Type-3 memory device；
2. 确认 openEuler guest 内核启用 CXL、DAX 和 Region 驱动；
3. 使用 `cxl-cli`/`daxctl` 创建 Region 和 `/dev/daxN.Y`；
4. 为 `neon` 用户配置最小设备访问权限；
5. 在 `cxl-cache` crate 中实现 `DaxBackend`；
6. 保持现有 Superblock、Region、Slot、sequence 和 Pagestream V4 接口不变；
7. 重新执行正确性、fallback、重启和性能验证。

参考资料：

- [Neon 仓库构建依赖](README.md)
- [QEMU CXL Type-3 文档](https://qemu.readthedocs.io/en/v10.0.3/system/devices/cxl.html)
- [Linux CXL DAX Driver 文档](https://docs.kernel.org/driver-api/cxl/linux/dax-driver.html)
