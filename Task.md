# Neon 单机 Ubuntu 容器部署计划

## 概要

在 Windows + Docker Desktop 上创建一个 Ubuntu 24.04 开发容器，运行完整 Neon 本地开发拓扑：broker、pageserver、单 safekeeper 和单 PostgreSQL compute。源码从宿主机挂载，数据库数据与编译缓存使用 Docker 命名卷持久化，仅向本机开放 PostgreSQL 端口。

该方案面向源码开发和功能实验，不作为生产或高可用部署。实施依据为 [Neon 官方本地开发流程](https://github.com/neondatabase/neon)。

## 关键实施

- 在当前空工作区递归克隆 `neondatabase/neon`，包含 PostgreSQL 等子模块，并固定到提交 `8f60b04da47ffefe0e52bda2440134b42874eb75`；将固定版本记录在部署配置和说明文档中。
- 增加 Ubuntu 24.04 `Dockerfile`：
  - 安装官方列出的编译依赖、Git、curl、Python 3.11+、Poetry 1.8+、protobuf、PostgreSQL client。
  - 创建非 root 开发用户，安装 rustup，并由仓库 `rust-toolchain.toml` 决定 Rust 版本。
  - 不将源码复制进镜像，镜像只提供稳定的工具链。
- 增加 Compose 配置：
  - 源码目录挂载到固定且无空格的 `/workspace/neon`。
  - `neon-data` 挂载到 `/workspace/neon/.neon`。
  - `cargo-cache`、`cargo-target` 和 `pg-install` 使用独立命名卷，避免 Windows bind mount 拖慢大型编译。
  - PostgreSQL 映射为 `127.0.0.1:55432:55432`；pageserver、safekeeper和管理端口不暴露到宿主机。
  - 配置约 8 CPU、16 GB 内存的 Docker Desktop 资源基线。
- 增加容器管理脚本：
  - `build`：执行 `make -j8 -s`，保留完整构建产物缓存。
  - `init`：仅在 `.neon` 未初始化时执行 `cargo neon init`、创建并设定默认 tenant、创建 `main` endpoint。
  - `start`：执行 `cargo neon start` 和 `cargo neon endpoint start main`；重复执行时安全处理“已存在/已运行”状态。
  - `stop`：先执行 `cargo neon stop`，再停止容器；容器收到 SIGTERM 时也进行同样的优雅退出。
  - `reset`：默认只停止服务；删除 `neon-data` 必须通过单独、明确的破坏性命令完成。
- 提供统一操作入口和 README：
  - 首次流程：构建镜像 → 编译源码 → 初始化数据 → 后台启动。
  - 日常流程：启动、停止、进入 shell、查看日志、执行 `psql`、重新编译。
  - 连接串固定为 `postgresql://cloud_admin@127.0.0.1:55432/postgres`。
  - 升级时切换到新的明确 SHA，递归更新子模块、重建并执行验证；升级前备份 `neon-data`。

## 验证与测试

- 镜像构建完成，容器内 `rustc`、`cargo`、`protoc`、`poetry` 和 `psql` 均可用。
- `make -j8 -s` 成功，Neon 和 patched PostgreSQL 产物位于持久化缓存卷。
- 启动后确认 broker、pageserver、safekeeper 和 `main` endpoint 均处于运行状态。
- 从 Windows 宿主机连接 `127.0.0.1:55432`，完成建表、写入和查询。
- 创建一个 Neon timeline 分支和第二个 endpoint，验证分支继承原数据且后续写入互相隔离。
- 停止并重建容器后，确认 tenant、timeline、endpoint 和测试数据仍存在。
- 验证数据库不能通过宿主机局域网地址访问，只有 `127.0.0.1` 可连接。
- 执行优雅停止后检查无残留 Neon 进程；重新启动后再次通过健康检查。

## 假设与默认值

- 宿主机是支持 Linux 容器的 Docker Desktop，已启用 Compose v2。
- Docker Desktop 至少分配 8 CPU、16 GB 内存，并预留约 50–100 GB 磁盘空间。
- 当前目录路径虽然包含中文，但不含空格；容器内部始终使用 ASCII、无空格路径。
- 采用 debug 构建以便源码调试；需要性能测试时再显式增加 release 构建配置。
- 使用单 pageserver、单 safekeeper、单 compute，不加入 Console、云控制平面、对象存储、高可用、TLS、公网暴露或生产备份体系。
