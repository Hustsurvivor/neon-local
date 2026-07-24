# Neon 单机多进程 CXL 共享缓存设计 V2

## 1. 文档状态

- 状态：设计草案
- 目标环境：单台 Linux 主机或单个 Linux 容器内的多进程 Neon
- 第一阶段后端：普通文件 + `mmap(MAP_SHARED)`
- 后续后端：CXL Type-3 设备对应的 `/dev/dax`
- 数据正确性原则：共享缓存可以随时丢失，任何缓存错误都必须回退到 Neon 原有 `GetPage` 路径

本文档是 `cxl_cache_design.md` 的实现级补充。V2 不要求第一阶段运行于
QEMU，也不要求存在真实 CXL 或 DAX 设备。普通共享文件用于模拟所有进程能够访问的
CXL 3.0 内存窗口。

---

## 2. 目标与非目标

### 2.1 目标

1. 使用一个普通文件模拟 CXL 共享内存池。
2. 允许多个 Pageserver 和 Compute 进程通过独立的 `mmap` 访问同一份页面数据。
3. Pageserver 负责生成和发布页面，Compute 只读并复制页面到 PostgreSQL Buffer。
4. 避免在 Pagestream 响应中传输完整的 8 KiB 页面。
5. 共享缓存失效时自动回退，不影响数据库正确性和可用性。
6. 将文件映射逻辑封装为后端，使后续迁移到 `/dev/dax` 时不修改缓存协议和上层逻辑。
7. 所有 Neon 业务进程，包括 Compute、Pageserver 和缓存 daemon，均以非 root 用户运行。

### 2.2 非目标

第一阶段不实现：

- 跨物理主机共享；
- 缓存持久化和重启恢复；
- 在线扩容或缩容；
- Pageserver 之间动态借用 Region；
- 不可信租户之间的硬件级内存隔离；
- CXL fabric manager；
- CXL 一致性协议本身的仿真；
- 使用共享页面替代 PostgreSQL Buffer。

---

## 3. 总体架构

系统包含四类组件：

```text
┌───────────────────────────────────────────────────────────────┐
│                       单机 Neon                               │
│                                                               │
│  ┌────────────────────┐       Unix Domain Socket              │
│  │ cxl-cache-daemon   │◄─────────────────────────────┐        │
│  │                    │                              │        │
│  │ Pool 初始化        │                              │        │
│  │ Region 分配        │                              │        │
│  │ Epoch 管理         │                              │        │
│  └─────────┬──────────┘                              │        │
│            │                                         │        │
│            ▼                                         │        │
│  ┌──────────────────────────────────────────────┐    │        │
│  │ FileBackend                                 │    │        │
│  │ /workspace/neon/.neon/cxl-cache/pool.bin    │    │        │
│  │ mmap(MAP_SHARED)                            │    │        │
│  └───────────────┬──────────────────────────────┘    │        │
│                  │                                   │        │
│        ┌─────────┴──────────┐                        │        │
│        ▼                    ▼                        │        │
│  ┌──────────────┐     ┌──────────────┐               │        │
│  │ Pageserver 0 │     │ Pageserver 1 │───────────────┘        │
│  │ Region 0     │     │ Region 1     │                        │
│  │ Cache Index  │     │ Cache Index  │                        │
│  │ Writer       │     │ Writer       │                        │
│  └──────┬───────┘     └──────┬───────┘                        │
│         │ PageLocation       │ PageLocation                   │
│         └──────────┬─────────┘                                │
│                    ▼                                          │
│              ┌─────────────┐                                  │
│              │ Compute     │                                  │
│              │ CXL Reader  │                                  │
│              │ PostgreSQL  │                                  │
│              │ Buffer      │                                  │
│              └─────────────┘                                  │
└───────────────────────────────────────────────────────────────┘
```

控制路径仍然经过 Pagestream：

```text
Compute ── GetPage ──► Pageserver ── PageLocation ──► Compute
```

页面数据路径不经过 Pagestream：

```text
Pageserver ── publish ──► Shared File ◄── atomic read ── Compute
```

Compute 读取共享页面后，仍然将页面复制到 PostgreSQL Buffer：

```text
Shared File → 8 KiB copy → PostgreSQL Buffer → Query Processing
```

---

## 4. 后端抽象

缓存管理层不能直接依赖普通文件或 `/dev/dax`。实现中定义统一的共享内存后端：

```text
SharedMemoryBackend
    open(config) -> Backend
    map(offset, length, access) -> Mapping
    pool_size() -> u64
    pool_identity() -> PoolIdentity
```

第一阶段实现：

```text
FileBackend
    path
    open(O_RDWR)
    ftruncate(pool_size)
    mmap(MAP_SHARED)
```

后续实现：

```text
DaxBackend
    device_path
    open(O_RDWR)
    mmap(MAP_SHARED)
```

缓存层只保存设备内逻辑 offset，不保存虚拟地址。不同进程分别计算：

```text
local_address = mapping_base + absolute_offset
```

从 `FileBackend` 迁移到 `DaxBackend` 时，下列内容保持不变：

- Superblock；
- Region 和 Slot 布局；
- `PageLocation`；
- Pagestream V4；
- Pageserver Cache Manager；
- Compute Reader；
- sequence 校验和回退逻辑。

---

## 5. 共享文件

### 5.1 默认配置

实验环境默认值：

```text
pool file        = /workspace/neon/.neon/cxl-cache/pool.bin
daemon socket    = /workspace/neon/.neon/cxl-cache/daemon.sock
pool size        = 1 GiB
default region   = 256 MiB
page size        = 8192 bytes
slot metadata    = 64 bytes
format version   = 1
```

所有值均可配置。Pool 和 Region 大小必须是 4 KiB 的整数倍，页面数据起始位置必须
按 8 KiB 对齐。

共享文件由 `cxl-cache-daemon` 创建，权限为 `0660`，owner 和 group 必须允许
运行 Neon 的非 root 用户访问。daemon 使用独占文件锁防止两个实例同时管理同一 Pool。

共享文件不调用 `fsync`。它是易失缓存，不能作为数据库恢复或持久化依据。

### 5.2 文件布局

```text
0
├── Superblock                         4 KiB
├── Region Descriptor Table           固定大小
├── Alignment Padding
├── Region 0
│   ├── Region Header                 4 KiB
│   ├── Slot Metadata Array           slot_count × 64 B
│   ├── Alignment Padding
│   └── Page Data Array               slot_count × 8 KiB
├── Region 1
│   └── ...
└── End of Pool
```

### 5.3 Superblock

Superblock 至少包含：

```text
PoolSuperblock {
    magic:              [u8; 8]    // "NEONCXL\0"
    format_version:     u32
    header_length:      u32
    pool_uuid:          [u8; 16]
    pool_epoch:         u64
    pool_size:          u64
    page_size:          u32
    slot_meta_size:     u32
    max_regions:        u32
    backend_kind:       u32        // FILE=1, DAX=2
    reserved:           [...]
}
```

`pool_uuid` 标识一次 Pool 初始化。重新创建共享文件时必须生成新的 UUID。

`pool_epoch` 标识 daemon 管理周期。daemon 重新初始化 Pool 时递增。Compute 收到的
位置引用只有在 UUID 和 epoch 同时匹配时才有效。

### 5.4 Region Descriptor

```text
RegionDescriptor {
    region_id:          u32
    state:              u32        // FREE, ALLOCATED
    pageserver_id:      u64
    region_epoch:       u64
    region_offset:      u64
    region_size:        u64
    slot_count:         u32
    reserved:           [...]
}
```

Region 不允许重叠。Region 重新分配或 Pageserver 重新初始化时，
`region_epoch` 必须递增。

---

## 6. Slot 布局与原子控制字

### 6.1 Slot Metadata

每个 Slot 使用 64 字节、64 字节对齐的元数据：

```text
SlotMeta {
    control:            AtomicU64
    checksum:           u32
    length:             u32
    key_hash:           u64
    page_lsn:           u64
    reserved:           [u8; 32]
}
```

Rust 端使用：

```text
#[repr(C, align(64))]
```

Compute C 端必须使用相同的 C ABI 布局，并通过编译期断言检查：

```text
sizeof(SlotMeta) == 64
alignof(SlotMeta) == 64
offsetof(SlotMeta, control) == 0
```

共享格式是易失、单机格式。第一阶段限定：

- 64 位 Linux；
- 小端；
- lock-free 64 位原子操作；
- Pageserver 与 Compute 使用相同架构。

启动时如果 64 位原子操作不是 lock-free，必须禁用共享缓存并回退。

### 6.2 Control 编码

`control` 同时保存状态和 generation：

```text
bit 0       BUSY
bit 1       VALID
bit 2..63   GENERATION
```

状态定义：

```text
FREE:
    BUSY  = 0
    VALID = 0

WRITING:
    BUSY  = 1
    VALID = 0

VALID:
    BUSY  = 0
    VALID = 1
```

一个已经发布的位置引用携带完整的 `control` 值。Slot 每次复用都增加 generation。

### 6.3 页面数据的原子访问

仅使用普通 `memcpy` 配合 sequence 会在语言内存模型中产生并发数据竞争。为使第一阶段
实现具有明确的并发语义，8 KiB 页面被视为 1024 个对齐的 64 位原子 word：

```text
PageData {
    words: AtomicU64[1024]
}
```

Writer 使用 relaxed atomic store 写入每个 word；Reader 使用 relaxed atomic load
读取每个 word。`control` 的 release/acquire 操作负责发布和观察完整页面。

后续若 DAX 平台提供不同的安全访问原语，可在后端中替换页面复制实现，但不能改变
上层 sequence 校验规则。

---

## 7. cxl-cache-daemon

### 7.1 职责

daemon 只管理：

- Pool 创建和映射；
- Superblock；
- Region 分配和释放；
- Pageserver 与 Region 的所有权；
- Pool 和 Region epoch；
- Unix Domain Socket。

daemon 不管理：

- CacheKey；
- 页面重建；
- Slot 分配；
- Slot 淘汰；
- 页面 checksum；
- Pagestream 请求；
- Compute 读取。

### 7.2 IPC

IPC 使用 Unix Domain Socket。第一阶段采用带长度前缀的版本化消息，避免依赖行分隔。

```text
RequestHeader {
    protocol_version: u16
    message_type:     u16
    payload_length:   u32
}
```

核心请求：

```text
ALLOC_REGION {
    pageserver_id:   u64
    requested_size: u64
    process_nonce:  u64
}
```

返回：

```text
REGION_ALLOCATED {
    pool_uuid:       [u8; 16]
    pool_epoch:      u64
    region_id:       u32
    region_epoch:    u64
    region_offset:   u64
    region_size:     u64
    slot_count:      u32
}
```

释放请求：

```text
RELEASE_REGION {
    pageserver_id:   u64
    process_nonce:   u64
    region_id:       u32
    region_epoch:    u64
}
```

daemon 必须验证 Pageserver 身份、nonce 和 epoch，拒绝旧进程释放新进程的 Region。

### 7.3 生命周期

正常启动：

```text
daemon 创建或打开 Pool
    ↓
校验格式
    ↓
取得独占锁
    ↓
初始化 Superblock 和 Region Table
    ↓
监听 Unix Socket
```

第一阶段不支持 daemon 无损热重启。daemon 重启后：

1. 生成新的 `pool_uuid`，或显式递增 `pool_epoch`；
2. 所有旧 `PageLocation` 失效；
3. Compute 自动回退普通 `GetPage`；
4. Pageserver 重新申请 Region 后才能重新启用共享缓存。

---

## 8. Pageserver CXL Cache Manager

### 8.1 进程内索引

每个 Pageserver 只管理自己的 Region，并在普通 DRAM 中维护：

```text
HashMap<CacheKey, SlotId>
```

CacheKey 定义为：

```text
CacheKey {
    tenant_shard_id
    timeline_id
    page_key
    effective_lsn
}
```

必须使用 Pageserver 已经解析出的 `effective_lsn`，不能直接使用 Compute 传入的
`request_lsn`。`request_lsn = Lsn::Max` 或 `not_modified_since` 优化都可能导致请求值
与实际页面版本语义不同。

共享文件中不保存完整 CacheKey。`key_hash` 只用于诊断和一致性检查，不用于恢复索引。

### 8.2 初始化

Pageserver 启动后：

1. 连接 daemon；
2. 申请 Region；
3. 映射 Region；
4. 清空进程内索引；
5. 将所有 Slot 初始化为 `FREE`；
6. 启用 CXL Cache Manager。

Pageserver 重启后不恢复旧索引，旧 Region 内容全部视为无效。

### 8.3 Cache Hit

```text
lookup CacheKey
    ↓
找到 SlotId
    ↓
acquire load control
    ↓
control 是 VALID 且与索引记录一致？
    ├── 否：删除旧索引，返回 miss
    └── 是：返回 PageLocation
```

Pageserver 返回位置后不 Pin Slot。Slot 随后被复用也不会造成错误，因为 Compute 会在
复制前后校验 `control`。

### 8.4 Cache Miss 与发布

```text
Cache Miss
    ↓
调用 Neon 原有页面重建路径
    ↓
再次查询索引，消除并发重复插入
    ↓
选择 FREE Slot 或 CLOCK victim
    ↓
从索引删除 victim
    ↓
将 control CAS 为新 generation 的 WRITING
    ↓
以 relaxed atomic store 写入 1024 个 word
    ↓
计算并写入 CRC32C、length、key_hash、page_lsn
    ↓
release store control = VALID
    ↓
更新 CacheKey → SlotId 索引
```

索引只能在页面成功发布后加入。任何写入错误都必须让 Slot 回到 `FREE`，并继续向
Compute 返回普通页面。

### 8.5 淘汰

第一阶段使用 CLOCK：

- 每个 Slot 有进程内访问位；
- Cache Hit 设置访问位；
- CLOCK 扫描遇到已设置访问位时清零并跳过；
- 选中 victim 后先删除 CacheKey 索引，再将 Slot 切换到新的 `WRITING` generation；
- 不等待 Reader，也不维护 `reader_count`。

Compute 与淘汰并发时会观察到 control 变化，从而放弃共享副本并回退。

---

## 9. Pagestream V4

### 9.1 兼容原则

现有 V2/V3 协议保持不变。新增：

```text
pagestream_v4
```

V4 客户端可以接收普通 8 KiB 页面或共享位置。V2/V3 客户端永远只收到普通页面。

### 9.2 请求标志

V4 `GetPage` 请求增加：

```text
flags: u8
```

第一阶段定义：

```text
ALLOW_SHARED_PAGE = 0x01
```

Compute 首次请求设置该标志。共享读取失败后的回退请求清除此标志。

### 9.3 响应

保留现有：

```text
GetPageResponse {
    echoed_request
    page[8192]
}
```

新增后端消息 tag：

```text
GetPageSharedResponse = 106
```

响应内容：

```text
GetPageSharedResponse {
    echoed_request
    pool_uuid:          [u8; 16]
    pool_epoch:         u64
    region_id:          u32
    region_epoch:       u64
    slot_id:            u32
    expected_control:   u64
    absolute_offset:    u64
    length:             u32
    checksum_crc32c:    u32
}
```

Pagestream 字段沿用现有网络字节序。`absolute_offset` 是 Pool 内 offset，不是指针。

### 9.4 Pageserver 响应规则

Pageserver 只有在下列条件全部成立时才返回共享位置：

- 协议为 V4；
- 请求包含 `ALLOW_SHARED_PAGE`；
- Pageserver 已启用共享缓存；
- Pool 和 Region 有效；
- Slot 当前为 `VALID`；
- `length == 8192`。

其他情况返回普通 8 KiB 页面。

Cache Miss 时 Pageserver可以在页面重建后发布共享副本并直接返回位置。如果发布失败，
仍然返回刚刚重建出的普通页面。

---

## 10. Compute CXL Reader

### 10.1 启动

Compute 启动时：

1. 检查 CXL Cache 配置；
2. 以只读权限打开共享文件；
3. 使用 `MAP_SHARED` 映射；
4. 校验 magic、格式版本、页大小和文件长度；
5. 保存 `pool_uuid` 和 `pool_epoch`；
6. 选择 Pagestream V4。

任何一步失败都只禁用 CXL Reader，不阻止 PostgreSQL 启动。

Compute 必须以 `neon` 等非 root 用户运行。

### 10.2 读取算法

收到 `GetPageSharedResponse` 后：

```text
1. 校验 pool_uuid 和 pool_epoch
2. 校验 region_epoch、length 和 absolute_offset 边界
3. acquire load control_before
4. 要求 control_before == expected_control
5. 要求 BUSY=0 且 VALID=1
6. relaxed atomic load 1024 个 u64 到 PostgreSQL 本地 Buffer
7. acquire load control_after
8. 要求 control_after == control_before
9. 计算本地 Buffer 的 CRC32C
10. 要求 checksum 匹配
11. 成功返回页面
```

Compute 永远不能让 PostgreSQL 直接持有共享文件中的地址。

### 10.3 自动回退

下列情况触发回退：

- Pool 未映射；
- UUID 或 epoch 不匹配；
- offset 越界；
- length 不是 8192；
- control 不是预期值；
- Slot 正在写入；
- 复制前后 control 变化；
- checksum 不匹配；
-共享文件发生 I/O 或映射错误。

回退流程：

```text
共享读取失败
    ↓
为该请求生成新的 request_id
    ↓
重发 GetPage，flags 清除 ALLOW_SHARED_PAGE
    ↓
Pageserver 返回普通 8 KiB 页面
```

每次逻辑 GetPage 最多执行一次共享回退，禁止无限重试。

### 10.4 与 LFC 的关系

现有 Compute Local File Cache 保持在原有位置：

```text
PostgreSQL Buffer Miss
    ↓
LFC Lookup
    ├── Hit：直接返回
    └── Miss：发送 Pagestream GetPage
                    ↓
             Shared 或普通响应
                    ↓
             写入 PostgreSQL Buffer
                    ↓
             按现有策略写入 LFC
```

CXL Cache 的性能对照组必须包含启用 LFC 的 Neon。

---

## 11. 配置

### 11.1 daemon

```text
cxl-cache-daemon
    --backend file
    --pool-file /workspace/neon/.neon/cxl-cache/pool.bin
    --pool-size 1GiB
    --socket /workspace/neon/.neon/cxl-cache/daemon.sock
    --max-regions 4
```

### 11.2 Pageserver

```toml
[cxl_cache]
enabled = true
daemon_socket = "/workspace/neon/.neon/cxl-cache/daemon.sock"
region_size = "256MiB"
eviction_policy = "clock"
```

### 11.3 Compute

建议增加以下 GUC 或 Compute 配置：

```text
neon.cxl_cache_enabled = on
neon.cxl_cache_file = '/workspace/neon/.neon/cxl-cache/pool.bin'
neon.cxl_cache_fallback = on
```

默认值必须为关闭。只有显式启用并成功映射 Pool 时才使用 Pagestream V4 共享响应。

---

## 12. 故障处理

| 故障 | 行为 |
|---|---|
| daemon 未启动 | Pageserver 禁用共享缓存，普通 GetPage |
| daemon 崩溃 | 现有位置仅在 epoch 仍有效时可读；Pageserver停止发布新位置 |
| Pageserver 崩溃 | Compute 的旧位置通过 region epoch/control 校验，失败后回退 |
| Compute 崩溃 | 无 Pin，不需要回收共享状态 |
| 共享文件不存在 | Compute 和 Pageserver禁用缓存 |
| 文件被截断 | 边界校验或映射错误，禁用缓存并回退 |
| Slot 写入中崩溃 | control 保持 WRITING，不会被 Reader 接受 |
| 页面部分写入 | sequence 或 checksum 失败，回退 |
| Slot 快速复用 | generation/control 不匹配，回退 |
| Pool 重新创建 | UUID 不匹配，所有旧位置失效 |
| 协议不支持 V4 | 使用现有 V2/V3 |

共享缓存错误不得直接转换为 PostgreSQL 查询错误，除非后续普通 GetPage 本身也失败。

---

## 13. 安全边界

普通文件模拟阶段假设同一主机上的 Neon 进程属于可信执行域。

最低安全要求：

- Pool 文件不能 world-readable；
- daemon Socket 不能 world-writable；
- daemon 校验对端 Unix 凭据；
- 所有 offset 和 length 在使用前检查溢出与边界；
- Compute 只以只读方式映射 Pool；
- Pageserver 只能写入 daemon 分配给自己的 Region；
- 日志不能输出页面内容。

因为 Compute 能够映射整个 Pool，第一阶段不提供强租户隔离。若用于不可信多租户环境，
需要按 Region 暴露独立文件描述符、使用权限隔离或由 daemon 通过 `SCM_RIGHTS`
传递受限映射对象。

---

## 14. 指标

Pageserver 指标：

```text
pageserver_cxl_cache_lookup_total{result="hit|miss|stale"}
pageserver_cxl_cache_publish_total{result="ok|error"}
pageserver_cxl_cache_evictions_total
pageserver_cxl_cache_bytes_written_total
pageserver_cxl_cache_slots{state="free|valid|writing"}
pageserver_cxl_cache_response_total{kind="shared|inline"}
```

Compute 指标：

```text
compute_cxl_cache_read_total{result="hit|fallback"}
compute_cxl_cache_validation_failures_total{reason}
compute_cxl_cache_bytes_read_total
compute_cxl_cache_read_seconds
compute_cxl_cache_fallback_seconds
```

daemon 指标：

```text
cxl_cache_regions{state="free|allocated"}
cxl_cache_region_allocations_total{result}
cxl_cache_pool_bytes
cxl_cache_allocated_bytes
```

---

## 15. 测试方案

### 15.1 布局和协议测试

- Superblock、RegionDescriptor、SlotMeta 的大小和 offset；
- Rust/C ABI 一致性；
- 所有整数溢出和越界输入；
- Pagestream V4 编解码；
- V2/V3 行为不变；
- 非 lock-free 64 位原子环境自动禁用。

### 15.2 独立多进程测试

实现三个最小程序：

```text
cxl-cache-daemon
cxl-cache-writer
cxl-cache-reader
```

验证：

- 两个进程独立 mmap 后读取相同页面；
- 多 Reader 并发；
- Writer 发布前 Reader 不能成功；
- Slot 高频复用时 Reader 不返回混合页面；
- generation wrap 之外的旧引用全部失效；
- checksum 能检测故意破坏的数据；
- 两个 Pageserver Region 互不覆盖。

### 15.3 故障测试

- 在写完 0%、25%、50%、100% 页面时杀死 Writer；
- Reader 复制过程中复用 Slot；
- daemon 重启；
- Pageserver 重启；
- Compute 重启；
- 删除、截断、替换 Pool 文件；
- 修改 UUID、epoch、control 和 checksum；
- Unix Socket 拒绝连接。

所有缓存故障都必须产生普通 GetPage 回退，不能返回损坏页面。

### 15.4 Neon 集成测试

- Cache Miss 后发布并返回共享位置；
- 第二次 GetPage 命中；
- `Lsn::Max` 和明确 LSN；
- 相同页面不同 LSN；
- 多 tenant、timeline 和 shard；
- Pagestream pipelining 和 batch；
- LFC Hit 不访问 CXL；
- CXL Hit 后按现有策略填充 LFC；
- V3 Compute 连接 V4 Pageserver；
- V4 Compute 连接未启用 CXL 的 Pageserver；
- 非 root Compute 正常启动和读取。

### 15.5 性能测试

至少比较：

1. Neon 原始 GetPage；
2. Neon + LFC；
3. Neon + FileBackend CXL Cache；
4. 后续 Neon + DaxBackend。

观测：

- GetPage P50/P95/P99；
- 查询吞吐量；
- Pagestream 网络字节数；
- Pageserver 页面重建次数；
- CXL Cache Hit Rate；
- atomic copy 成本；
- checksum 成本；
- fallback 比例；
- 1、2、4 个 Compute 并发时的扩展性。

---

## 16. 实施阶段

### 阶段 A：文件布局和独立原型

- 定义共享 ABI；
- 实现 FileBackend；
- 实现 daemon Region 分配；
- 实现 writer/reader；
- 完成 sequence、checksum 和故障测试。

完成标准：独立进程压力测试中不能出现未检测到的数据破坏。

### 阶段 B：Pageserver 集成

- 增加配置；
- 实现 CXL Cache Manager；
- 在 GetPage 的 effective LSN 已确定后查询缓存；
- 在页面重建完成后发布；
- 增加 CLOCK 和 Pageserver 指标。

完成标准：关闭功能时现有 Pageserver 测试无回归。

### 阶段 C：Compute 和协议集成

- 实现 Pagestream V4；
- 新增共享位置响应；
- Compute 映射 Pool；
- 实现原子复制、校验和单次回退；
- 保持 LFC 行为。

完成标准：任何人为注入的共享缓存错误都能回退普通 GetPage。

### 阶段 D：本地部署与性能实验

- 在本地 Docker 启动 daemon；
- 确保 Pageserver、Compute、daemon 以非 root 用户运行；
- 增加启停和状态检查；
- 完成多 Compute 基准测试。

### 阶段 E：迁移 `/dev/dax`

- 实现 DaxBackend；
- 保留相同共享 ABI；
- 检查 DAX 对齐、原子性和缓存一致性要求；
- 重跑全部正确性与性能测试。

---

## 17. 验收标准

设计实现完成需要同时满足：

1. 禁用 CXL Cache 时，Neon 行为和现有协议完全不变。
2. 启用 FileBackend 时，Pageserver 与 Compute 可以通过普通文件共享页面。
3. Compute 不会接受 WRITING、过期、越界或 checksum 错误的页面。
4. Slot 复用不需要 Pin/Unpin，也不会产生崩溃后的引用泄漏。
5. 所有共享缓存错误都能自动回退普通 GetPage。
6. Pageserver 和 Compute 重启不会把旧页面当作有效页面。
7. 多 Pageserver 的 Region 不重叠。
8. Compute、Pageserver 和 daemon 均不要求 root 权限。
9. 文件后端替换为 DAX 后端时，不改变 Pagestream 和缓存管理接口。

---

## 18. 总结

V2 将第一阶段的 CXL 设备具体化为普通共享文件：

```text
普通文件
    ↓ mmap(MAP_SHARED)
模拟 CXL 共享内存
    ↓
Pageserver 发布不可变 Page@LSN
    ↓
Pagestream 只返回 PageLocation
    ↓
Compute 原子复制并校验 sequence + checksum
    ↓
失败时自动回退普通 GetPage
```

与原设计相比，V2 的关键变化是：

- 不依赖 QEMU 和 `/dev/dax`；
- 增加可替换的共享内存后端；
- 使用 sequence 校验代替 Reader Pin/Unpin；
- 避免 Compute 崩溃造成 Pin 泄漏；
- 明确定义 Pagestream V4 和兼容回退；
- 明确定义 Rust/C 共享 ABI 和原子页面复制；
- 保留未来迁移真实 CXL Type-3 设备的路径。

第一阶段的目标不是模拟 CXL 硬件时序，而是验证 Neon 在“Pageserver 写共享内存、
Compute 直接读取共享内存”这一数据路径下的正确性、兼容性和性能收益。
