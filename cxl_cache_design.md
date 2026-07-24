# Neon 单主机多进程 CXL 共享缓存设计

## 1. 设计目标

本设计面向单台主机、多进程部署 Neon 的场景，通过 QEMU 模拟 CXL 内存，并使用共享内存映射模拟多个进程对 CXL 共享内存的访问。系统通过 `cxl-cache-daemon` 管理整个 CXL 内存池，并为不同 Pageserver 分配相互独立的 CXL 内存区域。每个 Pageserver 内部维护自己的 CXL Cache Manager，负责管理所属内存区域中的页面、Slot 和缓存状态。

该设计的核心目标是在不改变 Neon 原有数据持久化和事务机制的前提下，将 CXL 内存作为 Pageserver 和 Compute 之间的共享页面缓存。当 Compute 请求一个页面时，首先向对应 Pageserver 发起 `GetPage` 请求。Pageserver 查询自己的 CXL Cache，如果页面已经存在，则返回页面在 CXL 中的位置；如果不存在，则按照原有机制重建页面，将页面写入自己的 CXL Region，然后通知 Compute 直接从 CXL 读取页面。

CXL 内存仅作为可丢失、可重建的缓存使用，不承担持久化职责。即使 CXL 内存中的全部页面丢失，系统仍然可以通过 Neon 原有的 Pageserver、WAL 和对象存储机制重新获取页面，因此 CXL 缓存只影响性能，不影响数据正确性。

---

## 2. 整体架构

系统由 `cxl-cache-daemon`、多个 Pageserver、多个 Compute 以及 CXL 共享内存池组成。

`cxl-cache-daemon` 负责管理整个 CXL 内存池的资源分配。它不参与页面级别的缓存管理，不维护 CacheKey、Slot 状态、页面有效性、引用计数或淘汰策略。每个 Pageserver 启动后向 daemon 申请一块固定大小的 CXL Region，daemon 返回该 Region 在 CXL 内存池中的 offset 和 size。不同 Pageserver 获得的 Region 互不重叠，因此每个 Pageserver 可以独立管理自己的 CXL 内存。

Pageserver 内部包含 CXL Cache Manager。CXL Cache Manager 负责管理 Pageserver 所属 Region 中的页面，包括 CacheKey 到 Slot 的映射、Slot 分配、页面写入、页面发布、页面失效和缓存淘汰。Pageserver 是自己 CXL Region 的唯一管理者，daemon 不感知 Region 内部的页面组织方式。

Compute 不参与 CXL 内存管理。Compute 在执行 `GetPage` 时首先向对应 Pageserver 查询页面。如果 Pageserver 的 CXL Cache 命中，则 Pageserver 返回页面所在的 Region、Slot、generation 等元数据，Compute 随后直接访问 CXL 共享内存，将页面复制到 PostgreSQL Buffer 中。如果缓存未命中，则 Pageserver 按照原有 Neon 流程重建页面，写入自己的 CXL Region，并返回页面位置。

整体结构如下：

```text
                              单机 Neon
┌─────────────────────────────────────────────────────────────┐
│                                                             │
│  ┌─────────────────────┐                                    │
│  │  cxl-cache-daemon   │                                    │
│  │                     │                                    │
│  │ CXL Memory Manager  │                                    │
│  │ Region Allocation   │                                    │
│  │ Region Ownership    │                                    │
│  └──────────┬──────────┘                                    │
│             │                                               │
│             │ 分配 CXL Region                                │
│             │                                               │
│      ┌──────┴──────────────────────────────┐                │
│      │                                     │                │
│      ▼                                     ▼                │
│ ┌───────────────┐                    ┌───────────────┐      │
│ │ Pageserver 0  │                    │ Pageserver 1  │      │
│ │               │                    │               │      │
│ │ Page Rebuild  │                    │ Page Rebuild  │      │
│ │ CXL Writer    │                    │ CXL Writer    │      │
│ │ Cache Manager │                    │ Cache Manager │      │
│ │               │                    │               │      │
│ │ Region 0      │                    │ Region 1      │      │
│ └───────┬───────┘                    └───────┬───────┘      │
│         │                                    │              │
│         │ 管理 Region 0                       │ 管理 Region 1 │
│         │                                    │              │
│         └────────────────┬───────────────────┘              │
│                          ▼                                  │
│               ┌──────────────────────┐                      │
│               │    CXL Memory Pool   │                      │
│               │                      │                      │
│               │  Region 0            │                      │
│               │  Pageserver 0        │                      │
│               │                      │                      │
│               │  Region 1            │                      │
│               │  Pageserver 1        │                      │
│               └──────────────────────┘                      │
│                          ▲                                  │
│                          │                                  │
│                  Direct CXL Access                         │
│                          │                                  │
│              ┌───────────┴───────────┐                      │
│              │                       │                      │
│        ┌─────┴──────┐         ┌──────┴─────┐                │
│        │  Compute 0 │         │  Compute 1 │                │
│        │   Reader   │         │   Reader   │                │
│        └────────────┘         └────────────┘                │
│                                                             │
└─────────────────────────────────────────────────────────────┘
````

系统的数据路径和控制路径相互分离。Compute 与 Pageserver 之间传递的是页面位置和缓存状态等控制信息，而不是完整页面数据。页面真正的数据传输发生在 Compute 与 CXL 内存之间。Pageserver 负责告诉 Compute 页面在哪里，Compute 获得合法的页面引用后直接从 CXL 读取页面。

---

## 3. CXL 共享内存池

CXL 共享内存池是整个系统的数据存储区域。在 QEMU 模拟环境中，可以通过宿主机文件映射的方式创建一块共享内存文件，并将其作为 QEMU CXL Type-3 设备的后端内存。Guest 中通过 CXL 设备将该内存暴露给用户态，最终由不同进程通过 DAX 或 mmap 方式访问。

例如系统拥有 16 GiB CXL 内存：

```text
CXL Memory Pool
│
├── 0 GiB ─────────────── 8 GiB
│       Pageserver 0
│       Region 0
│
└── 8 GiB ────────────── 16 GiB
        Pageserver 1
        Region 1
```

每个 Pageserver 获得一个固定的 Region。Region 可以进一步划分为固定大小的 Slot，例如 8 KiB：

```text
Pageserver 0 Region

┌────────┬────────┬────────┬────────┬───────┐
│ Slot 0 │ Slot 1 │ Slot 2 │ Slot 3 │ ...   │
│  8KB   │  8KB   │  8KB   │  8KB   │       │
└────────┴────────┴────────┴────────┴───────┘
```

Slot 的大小建议与 Neon 页面大小保持一致，从而实现一个 Slot 保存一个完整的 `Page@LSN`。Pageserver 可以根据自己的需求决定 Slot 的具体组织方式，也可以在后续实验中使用其他内存分配策略。

CXL 内存中的页面数据不需要在多个进程之间复制。Pageserver 将页面写入 CXL 后，Compute 通过 mmap 映射同一 CXL 内存区域，并根据 Pageserver 返回的 offset 定位到相同的页面。

不同进程的虚拟地址可能不同，因此系统不能在进程之间传递裸指针。所有跨进程的页面位置都必须使用设备内的逻辑 offset 表示：

```text
CXL Physical / Device Offset
    =
Region Offset
    +
Slot Offset
```

每个进程根据自己的 mmap 基地址计算本地虚拟地址：

```text
Local Address
    =
Mapping Base
    +
CXL Device Offset
```

---

## 4. cxl-cache-daemon

`cxl-cache-daemon` 是 CXL 内存资源管理服务，负责整个 CXL Memory Pool 的初始化和 Region 分配。

Daemon 启动后负责建立 CXL 内存池的逻辑视图，并维护当前已经分配给不同 Pageserver 的 Region。Pageserver 启动时通过 Unix Domain Socket 等 IPC 机制向 daemon 申请内存：

```text
ALLOC_REGION {
    pageserver_id,
    requested_size
}
```

Daemon 返回：

```text
REGION {
    region_id,
    pageserver_id,
    offset,
    size
}
```

例如：

```text
Pageserver 0
    Region ID = 0
    Offset    = 0 GiB
    Size      = 8 GiB

Pageserver 1
    Region ID = 1
    Offset    = 8 GiB
    Size      = 8 GiB
```

Daemon 只维护 Region 级别的信息：

```text
Region 0 → Pageserver 0
Region 1 → Pageserver 1
```

它不维护：

```text
PageKey
LSN
Slot
CacheKey
generation
reader_count
页面有效性
页面淘汰
```

因此，daemon 不需要参与具体页面的读写过程。

Daemon 的核心职责是保证不同 Pageserver 获得的 Region 不发生重叠，并负责 Region 生命周期管理。当 Pageserver 退出时，可以释放对应 Region；当 Pageserver 重启时，可以重新申请 Region。由于 CXL 只作为缓存使用，Pageserver 重启后可以直接将自己的 Region 视为冷缓存并重新建立页面索引。

Daemon 不参与 Compute 的 `GetPage` 流程。Compute 不需要向 daemon 查询页面，也不需要知道其他 Pageserver 的 Region 信息。

---

## 5. Pageserver

Pageserver 是页面数据的生产者，也是自己 CXL Region 的缓存管理者。

Pageserver 启动后首先从 `cxl-cache-daemon` 获取自己的 Region。随后 Pageserver 将对应的 CXL 内存区域映射到自己的地址空间，并初始化内部的 CXL Cache Manager。

Pageserver 内部的缓存索引维护如下关系：

```text
CacheKey
    ↓
Page Location
    ↓
Region Offset + Slot Offset
```

其中 CacheKey 至少需要能够唯一标识一个特定版本的页面，建议使用：

```text
TenantId
TimelineId
PageKey
LSN
```

因为同一个逻辑页面在不同 LSN 下可能存在多个版本，所以不能只使用 PageKey 作为缓存索引。

Pageserver 查询页面时，如果缓存命中，则可以直接获得：

```text
PageLocation {
    region_id,
    page_offset,
    length,
    generation
}
```

如果缓存未命中，Pageserver 按照 Neon 原有机制从 Layer、WAL 或其他存储路径重建页面。页面重建完成后，Pageserver 从自己的 Region 中分配 Slot，将页面写入 CXL，并更新 Cache Index。

Pageserver 不需要将 CXL 页面内容通过 RPC 再传输给 Compute，而只需要向 Compute 返回页面位置。Compute 随后直接访问 CXL。

---

## 6. CXL Cache Manager

CXL Cache Manager 是 Pageserver 内部负责页面缓存管理的核心组件。

它只管理当前 Pageserver 所属的 CXL Region，不知道其他 Pageserver 的缓存状态。其主要数据结构包括页面索引、Slot 元数据、空闲 Slot 管理和缓存淘汰信息。

页面索引：

```text
CacheKey
    ↓
SlotId
```

Slot 元数据：

```text
SlotMeta {
    state
    generation
    reader_count
}
```

Slot 状态定义为：

```text
FREE
WRITING
VALID
EVICTING
```

一个 Slot 的正常生命周期为：

```text
FREE
  ↓ allocate
WRITING
  ↓ write complete + publish
VALID
  ↓ evict
EVICTING
  ↓ reader_count == 0
FREE
```

Writer 申请 Slot 后，状态从 `FREE` 变为 `WRITING`。页面写入完成后，只有在所有数据已经写入 CXL 后，才能将状态原子地修改为 `VALID`。Reader 只有看到 `VALID` 状态后才能读取页面。

这样可以避免 Compute 读取到正在写入的页面。

---

## 7. Writer

Writer 是 Pageserver 内部负责将页面写入 CXL Cache 的组件。

当 Pageserver 完成一个页面的重建后，Writer 根据 `CacheKey` 查询 CXL Cache Manager。如果页面已经存在，则不需要重复写入。如果页面不存在，则申请一个空闲 Slot。

Writer 首先将 Slot 状态设置为：

```text
WRITING
```

然后将完整的页面数据写入 CXL：

```text
CXL Slot
    ←
8 KiB Page
```

页面写入完成后，可以执行 checksum 校验，并通过 release memory ordering 发布：

```text
state = VALID
```

Reader 使用 acquire memory ordering 读取 Slot 状态。只有 Reader 观察到 `VALID` 后，才允许访问页面数据。

逻辑上：

```text
Writer:

allocate Slot
    ↓
state = WRITING
    ↓
write 8 KiB page
    ↓
checksum
    ↓
publish
    ↓
state = VALID
```

这样可以保证：

```text
VALID
```

意味着：

```text
页面数据已经完整写入 CXL
```

而不是页面正在写入过程中就被 Reader 读取。

---

## 8. Reader

Reader 位于 Compute 的页面读取路径中。

Compute 请求页面时首先向对应 Pageserver 发起：

```text
GetPage(PageKey, LSN)
```

Pageserver 查询自己的 Cache Index。

如果命中，Pageserver 返回：

```text
PageLocation {
    region_id,
    page_offset,
    length,
    generation
}
```

Compute 根据 CXL mmap 地址和 `page_offset` 计算页面地址，然后直接从 CXL 读取数据：

```text
CXL Address
    =
Mapping Base
    +
Region Offset
    +
Page Offset
```

随后将页面复制到 PostgreSQL Buffer。

Compute 不应该长期直接使用 CXL 中的页面地址。CXL Cache 中的页面属于共享缓存，而 PostgreSQL Buffer 才是数据库执行过程中的工作副本。

因此数据路径为：

```text
CXL Shared Page
      ↓
memcpy
      ↓
PostgreSQL Buffer
      ↓
Query Processing
```

Compute 对 CXL 页面只执行读取，不直接修改 CXL 中的缓存页面。

---

## 9. Reader Pin 与 Slot 生命周期

由于 Compute 获取页面位置后会绕过 Pageserver直接访问 CXL，因此必须保证 Compute 读取期间对应 Slot 不会被 Pageserver 回收和复用。

系统采用 Reader Pin 机制解决该问题。

Compute 在获得页面位置时，同时向 Pageserver 请求 Pin：

```text
GetPageAndPin(PageKey, LSN)
```

Pageserver 查找到对应 Slot 后：

```text
reader_count += 1
```

然后返回页面位置：

```text
PageLocation {
    region_id,
    page_offset,
    generation
}
```

此时该 Slot 被 Pin。

Pageserver 的淘汰逻辑必须保证：

```text
reader_count > 0
```

时不能回收 Slot。

Compute 完成 CXL 页面读取后发送：

```text
Unpin(SlotId, generation)
```

Pageserver 执行：

```text
reader_count -= 1
```

只有：

```text
state == EVICTING
AND
reader_count == 0
```

时，Slot 才能真正变成：

```text
FREE
```

完整流程：

```text
Compute
    │
    │ GetPageAndPin
    ▼
Pageserver
    │
    │ Lookup Cache
    │ reader_count++
    ▼
返回 PageLocation
    │
    ▼
Compute
    │
    │ Direct Read
    ▼
CXL Memory
    │
    ▼
PostgreSQL Buffer
    │
    │ Unpin
    ▼
Pageserver
    │
    │ reader_count--
    ▼
Slot 可回收
```

Pin 解决的是 Compute 正在读取页面时 Slot 不被复用的问题。

---

## 10. Generation 机制

每个 Slot 都维护一个单调递增的 generation。

例如：

```text
Slot 0
generation = 10
Page A@LSN100
```

当页面被淘汰并重新使用：

```text
Slot 0
generation = 11
Page B@LSN200
```

Compute 获取页面位置时，同时获得：

```text
SlotId = 0
Generation = 10
```

如果后续发现：

```text
Current Generation != Expected Generation
```

则说明当前引用已经过期。

Generation 主要用于检测旧引用误用，防止 Slot 被重新分配后，旧的 `PageLocation` 继续指向新的页面。

Reader Pin 和 generation 分别解决两个问题：

```text
Reader Pin
    ↓
保证当前正在读取的 Slot 不被回收

Generation
    ↓
检测已经失效的旧 Slot 引用
```

两者需要同时使用。

---

## 11. 原子状态发布

Writer 和 Reader 之间必须建立明确的内存可见性保证。

Writer 写入页面：

```text
state = WRITING

memcpy(CXL_Slot, Page, 8192)

memory barrier

state = VALID
```

Reader：

```text
state = acquire_load()

if state != VALID:
    Cache Miss

else:
    memcpy(LocalBuffer, CXL_Slot, 8192)
```

其中：

```text
WRITING → VALID
```

必须是发布页面的原子状态转换。

Reader 观察到：

```text
VALID
```

后，必须能够看到 Writer 在设置 `VALID` 之前对页面数据的完整写入。

因此：

```text
写页面
  ↓
Release
  ↓
VALID

VALID
  ↓
Acquire
  ↓
读页面
```

这样可以避免 Reader 读取到半写入页面。

---

## 12. GetPage 完整流程

Compute 请求页面：

```text
GetPage(PageKey, LSN)
```

首先向对应 Pageserver发送请求：

```text
Compute
    │
    │ GetPageAndPin
    ▼
Pageserver
```

Pageserver 查询 CXL Cache Index。

如果命中：

```text
Cache Hit
    ↓
检查 Slot == VALID
    ↓
检查 generation
    ↓
reader_count++
    ↓
返回 PageLocation
```

Compute 获得位置后直接读取 CXL：

```text
Compute
    ↓
CXL mmap
    ↓
Region Offset + Page Offset
    ↓
读取 8 KiB
    ↓
复制到 PostgreSQL Buffer
```

读取完成：

```text
Compute
    │
    │ Unpin
    ▼
Pageserver
```

如果 Cache Miss：

```text
Compute
    │
    │ GetPage
    ▼
Pageserver
    │
    │ Cache Miss
    ▼
页面重建
    │
    ▼
Allocate Slot
    │
    ▼
state = WRITING
    │
    ▼
写入 CXL
    │
    ▼
state = VALID
    │
    ▼
更新 Cache Index
    │
    ▼
Pin
    │
    ▼
返回 PageLocation
    │
    ▼
Compute 直接读取 CXL
```

---

## 13. Cache Miss 与故障回退

CXL Cache 必须设计为非关键路径。

以下情况都应视为 Cache Miss：

```text
CXL Cache Miss
Pageserver Cache Manager 不可用
Slot 状态不是 VALID
Generation 不匹配
Checksum 校验失败
CXL 映射失败
Pageserver 重启导致 epoch 失效
```

发生上述情况时，Compute 不应该直接返回错误，而应该回退到 Neon 原有的页面获取流程。

例如：

```text
Compute
    │
    │ CXL Cache Lookup
    ▼
Cache Miss / Invalid
    │
    ▼
Pageserver GetPage
    │
    ▼
正常页面获取
```

这样可以保证：

```text
CXL Cache 发生故障
        ↓
性能下降
        ↓
不会导致数据库数据错误
```

---

## 14. Pageserver 重启与缓存恢复

CXL Cache 是易失缓存，不要求在 Pageserver 重启后恢复原有缓存索引。

Pageserver 重启后：

```text
Pageserver
    ↓
重新向 daemon 申请 Region
    ↓
初始化 Cache Manager
    ↓
清空 Cache Index
    ↓
所有 Slot 设置为 FREE
```

原来的 CXL 页面可以直接视为无效。

如果需要防止旧 Compute 继续使用旧页面引用，可以为每个 Pageserver维护一个 `partition_epoch`。

Pageserver 每次初始化 Region 时增加 epoch：

```text
第一次启动：
epoch = 1

重启：
epoch = 2
```

Compute 获得的 PageLocation 包含：

```text
PageLocation {
    pageserver_id,
    partition_epoch,
    slot_id,
    generation
}
```

如果 Compute 发现：

```text
expected_epoch != current_epoch
```

则认为页面引用已经失效，重新执行 GetPage。

---

## 15. 多 Pageserver 隔离

每个 Pageserver 的 CXL Region 完全独立。

例如：

```text
CXL Pool = 16 GiB

Region 0
    Pageserver 0
    [0 GiB, 8 GiB)

Region 1
    Pageserver 1
    [8 GiB, 16 GiB)
```

Pageserver 0 只允许访问 Region 0。

Pageserver 1 只允许访问 Region 1。

不同 Pageserver 不共享 Slot，不共享 Cache Index，也不共享页面状态。

因此：

```text
Pageserver 0
    │
    └── Cache Manager 0
            │
            └── Region 0

Pageserver 1
    │
    └── Cache Manager 1
            │
            └── Region 1
```

不同 Pageserver 之间完全解耦。

该设计的主要代价是 CXL 空间不能动态共享。例如 Pageserver 0 的 Region 已经使用完毕，而 Pageserver 1 仍有大量空闲空间时，Pageserver 0 不能直接使用 Pageserver 1 的 Region。

在单机实验阶段可以采用静态容量配置：

```text
Pageserver 0: 8 GiB
Pageserver 1: 8 GiB
```

后续如果需要动态调整容量，可以由 daemon 增加 Region 扩容接口，但这不属于第一阶段的必要功能。

---

## 16. 核心接口

### cxl-cache-daemon

```text
ALLOC_REGION(
    pageserver_id,
    size
) -> RegionInfo
```

```text
RELEASE_REGION(
    pageserver_id,
    region_id
)
```

返回：

```text
RegionInfo {
    region_id,
    offset,
    size
}
```

### Pageserver Cache Manager

```text
ALLOC_SLOT()
    -> SlotId
```

```text
PUBLISH_PAGE(
    slot_id,
    generation,
    page_data
)
```

```text
LOOKUP_AND_PIN(
    cache_key
)
    -> PageLocation
```

```text
UNPIN(
    slot_id,
    generation
)
```

```text
EVICT(
    slot_id,
    generation
)
```

### Compute

```text
GetPageAndPin(
    page_key,
    lsn
)
    -> PageLocation
```

```text
ReadCXL(
    PageLocation
)
    -> PageData
```

```text
Unpin(
    PageLocation
)
```

---

## 17. 最小验证程序

在正式接入 Neon 之前，建议先实现三个最小组件：

```text
cxl-cache-daemon
writer
reader
```

Daemon 首先模拟 Region 分配：

```text
Daemon
    ↓
Region 0 → Writer
Region 1 → Reader
```

Writer 将页面写入指定 Region：

```text
Writer
    ↓
Slot 0
    ↓
写入 Page A
    ↓
Publish
```

Reader 根据 Writer 提供的：

```text
Region Offset
Page Offset
Generation
```

直接读取 CXL：

```text
Reader
    ↓
mmap CXL
    ↓
定位 Page A
    ↓
读取
    ↓
校验 checksum
```

第二阶段加入多个 Writer 和 Reader，验证：

```text
多个进程同时读取
多个 Pageserver 独立 Region
Slot 并发分配
Writer / Reader 并发访问
Slot 淘汰
Reader Pin
Generation 校验
```

第三阶段再将 Writer 集成到 Pageserver，将 Reader 集成到 Compute：

```text
Pageserver
    │
    └── CXL Cache Manager
            │
            └── Writer

Compute
    │
    └── CXL Reader
```

最终验证完整链路：

```text
Compute GetPage
    ↓
Pageserver Cache Lookup
    ↓
CXL Cache Hit
    ↓
Pin
    ↓
返回 PageLocation
    ↓
Compute Direct Read CXL
    ↓
Copy to PostgreSQL Buffer
    ↓
Unpin
```

---

## 18. 测试要求

首先验证 CXL Region 隔离。启动两个 Pageserver，为它们分别分配不同 Region，Pageserver 0 写入 Region 0，Pageserver 1 写入 Region 1，验证两个 Pageserver 的数据互不覆盖。

其次验证跨进程共享。Writer 进程将固定模式的数据写入 CXL，Reader 进程通过独立的 mmap 映射读取相同 offset，验证 Reader 能够获得 Writer 写入的数据。

随后验证并发读写。Writer 在页面完全写入之前保持 Slot 为 `WRITING`，Reader 不应该读取该页面；只有状态变成 `VALID` 后 Reader 才允许访问。

验证 Slot 复用。Writer 写入 Page A 后将 Slot 淘汰，再将同一 Slot 分配给 Page B。验证旧的 generation 无法访问 Page B。

验证 Reader Pin。Reader 在读取 Slot 的过程中，另一个线程尝试淘汰该 Slot，验证 Slot 不会被立即复用；Reader Unpin 后 Slot 才能真正回收。

验证 Pageserver 崩溃恢复。Pageserver 重启后清空 Cache Index，旧的 `partition_epoch` 失效，Compute 必须回退到正常 GetPage 流程。

最终验证性能指标，包括 CXL Cache Hit Rate、Cache Miss Rate、Pageserver 页面重建次数、GetPage 延迟、CXL 直接读取延迟、Reader Pin/Unpin 开销以及不同 Pageserver 数量下的吞吐量。

---

## 19. 总结

本设计采用三层职责划分：

```text
cxl-cache-daemon
    ↓
管理 CXL 内存资源和 Region 所有权

Pageserver + CXL Cache Manager
    ↓
管理 Region 内的页面和 Slot 生命周期

Compute + CXL Reader
    ↓
根据 Pageserver 返回的 PageLocation 直接读取 CXL
```

其核心数据流为：

```text
                         Control Path

Compute ─────── GetPageAndPin ───────► Pageserver
   ▲                                      │
   │                                      │
   │                              Cache Lookup
   │                                      │
   │                                      ▼
   │                               PageLocation
   │                                      │
   │                                      │
   └────────────── PageLocation ──────────┘


                         Data Path

Compute
   │
   │ mmap
   │
   ▼
CXL Shared Memory
   │
   │ Direct Read
   ▼
PostgreSQL Buffer
```

整个设计的核心原则是：

**Daemon 管理内存 Region，Pageserver 管理页面，Compute 直接读取页面。**

Daemon 不参与页面级缓存管理；不同 Pageserver 的 CXL Region 相互隔离；Pageserver 自己维护 Slot、CacheKey、generation、页面状态和淘汰策略；Compute 通过 `GetPageAndPin` 获得合法的页面引用后直接访问 CXL；通过 `Reader Pin` 防止读取期间 Slot 被复用，通过 `generation` 检测旧引用，通过 `WRITING → VALID` 原子状态发布保证页面完整性。

在单机、多进程实验阶段，该架构能够以较低的实现复杂度验证 CXL 共享内存对 Neon 页面读取路径的加速效果，同时保持 Neon 原有的数据正确性和故障恢复机制。
