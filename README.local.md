# Neon 单机 Ubuntu 容器

本配置在 Windows + Docker Desktop 的 Ubuntu 24.04 容器中编译并运行 Neon
本地开发拓扑。它固定于提交
`8f60b04da47ffefe0e52bda2440134b42874eb75`，仅用于开发和功能实验。

## 前置条件

- Docker Desktop 使用 Linux containers，Compose v2 可用。
- Docker Desktop 至少分配 8 CPU、16 GB 内存。
- 预留 50–100 GB 磁盘空间。
- PowerShell 当前目录为本仓库根目录。

Windows Git 必须避免把 Linux 构建脚本转换为 CRLF。克隆或重新检出源码前设置：

```powershell
git config --global core.autocrlf false
```

源码 bind mount 到 `/workspace/neon`。`.neon` 数据、Cargo 下载缓存、Rust
构建产物和 patched PostgreSQL 安装目录分别存放在 Docker 命名卷中。
所有管理操作会在容器内自动降权为 `neon` 用户；compute 和 PostgreSQL
不会以 root 身份启动。

## 首次运行

```powershell
.\neon.ps1 first-run
```

该命令依次校验固定 SHA、构建 Ubuntu 镜像、验证工具链、执行
`make -j8 -s`、初始化持久化数据并启动 broker、pageserver、单 safekeeper
和 `main` compute。首次完整编译可能很久。

连接串：

```text
postgresql://cloud_admin@127.0.0.1:55432/postgres
```

只有 `127.0.0.1:55432` 映射到宿主机；存储及管理端口不向宿主机暴露。

## 日常操作

```powershell
.\neon.ps1 start
.\neon.ps1 status
.\neon.ps1 psql
.\neon.ps1 shell
.\neon.ps1 logs
.\neon.ps1 build
.\neon.ps1 stop
```

`reset` 与 `stop` 一样只停止服务并保留数据。删除数据库数据必须使用单独命令
并明确确认：

```powershell
.\neon.ps1 destroy-data --confirm
```

Cargo 与 PostgreSQL 构建缓存卷不会被上述命令删除。

## 分支隔离验证

主库中建表并写入数据后，在容器 shell 中执行：

```bash
cargo neon timeline branch --branch-name migration_check
cargo neon endpoint create migration_check --branch-name migration_check
cargo neon endpoint start migration_check
psql -h 127.0.0.1 -p 55434 -U cloud_admin postgres
```

第二个 endpoint 只在容器网络命名空间内监听，没有映射到 Windows。确认其继承
主分支数据后，在其中写入新行，再连接 55432 验证主分支没有该行。

## 升级

升级前备份 `neon-local-data`。切换到一个经过选择的明确提交，执行
`git submodule update --init --recursive`，同步修改 `compose.yaml`、
`neon.ps1` 与本文中的 SHA，然后重新执行镜像构建、源码构建和完整验证。

## 验证清单

1. `.\neon.ps1 toolchain` 显示 rustc、cargo、protoc、Poetry 和 psql。
2. `.\neon.ps1 status` 显示 `main` 为 running，Compose 健康检查通过。
3. 从 Windows 连接 55432，完成建表、写入和查询。
4. 完成上述分支隔离验证。
5. 停止并重新创建容器后确认 tenant、timeline、endpoint 和数据仍存在。
6. 检查端口绑定为 `127.0.0.1:55432`，局域网地址无法访问。
