# E2EE 增量版本管理系统 - 树状架构规格 v4

## 0. 文档定位

这份文档把前几版内容重组为树状架构规格。它不再按“问题补丁”的方式展开，而是按系统结构自顶向下描述：

```text
系统总架构
  -> 子架构
      -> 模块
          -> 设计要点
          -> 技术选型
          -> 扩展点
          -> 健壮性要求
          -> 阶段归属
```

目标是让评审者能快速回答四个问题：

1. 整个系统分成哪些子架构？
2. 每个子架构里有哪些模块？
3. 每个模块怎么设计、用什么技术？
4. 这些设计如何支撑扩展性、健壮性和阶段演进？

## 1. 总架构

### 1.1 系统定位

本系统是一个本地优先、端到端加密、支持增量快照和远端同步的数据版本管理系统。

它的长期目标是服务这些场景：

- 个人多设备异地备份。
- 工程项目和文档历史快照。
- 大型二进制文件，如模型权重、视频、设计素材。
- 海量小文件数据集，如图片集、标注数据、实验结果。
- 不信任远端存储的加密归档。
- 基于快照和分支的数据沙箱、变体管理和交付。

### 1.2 总体能力树

```text
E2EE 增量版本管理系统
  1. 数据平面
     1.1 文件扫描
     1.2 内容定义切块 CDC
     1.3 Keyed Hash 去重
     1.4 加密流水线
     1.5 Packfile

  2. 密码学平面
     2.1 密钥派生
     2.2 仓库主密钥
     2.3 对象加密
     2.4 Keyring
     2.5 多设备与团队密钥

  3. 元数据与版本平面
     3.1 Object Model
     3.2 Manifest Store
     3.3 Merkle DAG
     3.4 Snapshot
     3.5 Branch / Ref
     3.6 Local Index
     3.7 Read Service

  4. 存储与同步平面
     4.1 StorageLayout
     4.2 LogicalObjectStore
     4.3 BlobStore
     4.4 RefStore
     4.5 LayoutRootStore
     4.6 TransactionPublisher
     4.7 Backend Adapter
     4.8 Push / Fetch / Clone
     4.9 Operation Journal
     4.10 GC / Verify / Repair

  5. 访问与呈现平面
     5.1 CLI
     5.2 Local Web UI
     5.3 Local HTTP API
     5.4 VFS
     5.5 SDK / C-ABI

  6. 运行时与可靠性平面
     6.1 并发调度
     6.2 背压
     6.3 本地锁
     6.4 崩溃恢复
     6.5 可观测性
     6.6 测试矩阵
     6.7 格式版本边界
```

### 1.3 总体分层图

```text
=============================================================================
                         访问与呈现平面
      CLI        Local Web UI        Local HTTP API        VFS        SDK/C-ABI
=============================================================================
                                  |
                                  v
+---------------------------------------------------------------------------+
|                            编排层 / Facade                                  |
|     commit / checkout / push / fetch / clone / serve / verify / gc          |
+---------------------------------------------------------------------------+
                                  |
=============================== 核心领域层 ==================================
                                  |
+-----------------------------+       +-------------------------------------+
|          数据平面            |       |        元数据与版本平面              |
|  scan / CDC / hash / crypto  |       |  manifest / tree / snapshot / ref    |
|  chunk / pack / stream       |       |  branch / index / search             |
+-----------------------------+       +-------------------------------------+
                                  |
+---------------------------------------------------------------------------+
|                            密码学平面                                       |
|      keyring / KDF / AEAD / nonce / keyed object id / key rotation           |
+---------------------------------------------------------------------------+
                                  |
=============================================================================
                                  v
+---------------------------------------------------------------------------+
|                           存储与同步平面                                    |
| LogicalObjectStore  StorageLayout  BlobStore  RefStore  LayoutRoot  Journal |
+---------------------------------------------------------------------------+
                                  |
=============================================================================
                                  v
+---------------------------------------------------------------------------+
|                            后端适配器                                       |
|       Local Folder        S3-compatible        WebDAV/Alist        Memory    |
+---------------------------------------------------------------------------+
```

### 1.4 架构原则

- 核心对象不可变，只有 ref 可变。
- 内容和元数据都在本地加密。
- 远端只保存不可读对象和不可读引用。
- 本地索引是 cache，不是事实源。
- CLI/Web/VFS/SDK 都通过同一编排层访问核心能力。
- 存储后端通过能力声明接入，不把后端差异泄漏到核心领域。
- 核心领域只依赖逻辑 `ObjectId`，不得依赖远端物理路径、prefix list 或 object naming 规则。
- `StorageLayout` 负责逻辑对象到物理存储布局的映射，P0 使用 direct layout，P1 新增 pack layout，P3 新增 rewrite/oram layout 时不重写核心对象模型。
- P0 必须先建立 `Repository Facade`、`WorkingTree`、`LogicalObjectStore`、`ReadService` 和 `TransactionPublisher` 边界，再实现 CLI 命令，避免后续 Web/VFS/SDK 反向重写核心流程。
- Manifest、Web、VFS、SDK 不得直接访问 `BlobStore` 或拼接 physical path，只能通过 `LogicalObjectStore`、`ReadService` 和 `StorageLayout` 读写对象。
- ref 更新、layout root 发布、writer lease、write intent 和恢复逻辑必须由 `TransactionPublisher` 统一编排，不能散落在 CLI、Sync Engine 或后端适配器里。
- MVP 先证明对象模型、加密模型和同步一致性，再扩展 VFS、团队共享和高级 GC。

### 1.5 P0 防重构边界

P0 不是“先把 CLI 写通再抽象”。为了避免 P1/P2 大规模重构，P0 必须先固定这些薄边界：

```text
Repository Facade
  -> init / commit / checkout / snapshots / verify
  -> push / fetch / clone / serve

WorkingTree
  -> scan / path policy / source consistency / checkout writer

LogicalObjectStore
  -> canonical encode / object id / encrypt / decrypt / layout resolve

ReadService
  -> open snapshot / read dir / open file / read range

TransactionPublisher
  -> writer lease / write intent / layout root CAS / ref CAS / recovery
```

边界规则：

- CLI 只调用 `Repository Facade`，不得直接调用 chunker、manifest、BlobStore 或 RefStore。
- Web UI、Local HTTP API、VFS 和 SDK 只调用 `ReadService` 或稳定 facade，不得复制目录遍历、range read 和解密认证逻辑。
- Commit 通过 `WorkingTree` 获得稳定输入，通过 `LogicalObjectStore` 写对象，通过 Snapshot Manager 形成 DAG，再通过 facade 更新本地状态。
- Push 通过 `TransactionPublisher` 完成 write intent、layout root 和 ref 的两阶段发布，Sync Engine 不得自行拼接发布协议。
- P0 direct layout 可以非常薄，但必须实现同一接口；P1 pack、P3 rewrite/ORAM 只能替换 layout/object-store 实现，不能要求上层重写。

## 2. 阶段路线树

### 2.1 MVP / P0

```text
P0-A 本地加密快照引擎
  - Repository Facade
  - WorkingTree boundary
  - LogicalObjectStore direct layout
  - ReadService snapshot/dir/file/range API
  - local direct layout root/generation
  - init
  - commit
  - snapshots
  - checkout
  - local folder backend
  - FastCDC
  - keyed object ID
  - encrypted chunk/file/tree/snapshot
  - default branch/ref token
  - Tree Sharding if target includes 100k+ small files

P0-B 单远端同步
  - S3-compatible backend
  - push
  - fetch
  - clone
  - ref CAS
  - layout root publish protocol
  - TransactionPublisher
  - remote write intent
  - operation journal
  - upload resume

P0-C 本地 Web 浏览
  - local axum server
  - snapshot browse
  - directory browse
  - download
  - HTTP Range
```

### 2.2 P1

```text
P1-A 分支与本地索引
  - branch create/list/delete
  - checkout branch
  - SQLite index
  - metadata search
  - filename search
  - Tree Sharding if not already required by MVP scale target

P1-B WebDAV 与后端降级
  - WebDAV/Alist adapter
  - capability detection
  - single-writer lease fallback
  - risky write mode disabled by default

P1-C Packfile 与性能优化
  - pack writer
  - pack index
  - range read
  - local cache
  - benchmark
  - pack/index implementation behind LogicalObjectStore and StorageLayout

P1-D 显式 GC 与防腐
  - verify snapshot
  - verify remote sample
  - repair
  - gc dry-run
  - gc execute
```

### 2.3 P2

```text
P2-A 只读 VFS
  - P2-A1 Linux FUSE
  - P2-A2 Windows WinFSP
  - P2-A3 macOS macFUSE
  - read-only mount
  - range read
  - encrypted cache
  - plaintext memory cache

P2-B 多设备密钥与共享
  - device key envelope
  - device authorization
  - keyring update
  - limited team sharing

P2-C SDK / C-ABI
  - stable Rust API
  - opaque C handles
  - Local HTTP API stabilization
```

### 2.4 P3

```text

P3-A 历史强撤销
  - full repository re-encryption
  - epoch rewrite plan
  - old epoch retirement
  - encrypted object rewrite journal
  - layout rewrite root publish

P3-B 访问模式隐藏
  - ORAM-style storage layout
  - padded access schedule
  - traffic shaping
  - cost model and policy control
  - oblivious layout root

P3-C 可写 VFS
  - writable mount
  - conflict handling
  - atomic writeback
  - cache coherency
```

## 3. 数据平面架构

### 3.1 职责

数据平面负责把明文字节流转换为可存储、可去重、可校验、可加密的对象流。

它不理解目录结构，不决定分支历史，不直接更新 ref。

### 3.2 模块树

```text
Data Plane
  1. File Scanner
  2. Chunker
     2.1 FastCDC Chunker
     2.2 Chunk Profile
     2.3 P2 Semantic Chunker
     2.4 Chunking Policy Engine
  3. Hasher
     3.1 BLAKE3 keyed object ID
     3.2 Chunk fingerprint
  4. Encryptor
     4.1 AEAD envelope
     4.2 nonce derivation
  5. Stream Pipeline
     5.1 reader
     5.2 chunker
     5.3 hasher
     5.4 encryptor
     5.5 uploader
  6. Pack Writer
     6.1 loose object writer
     6.2 pack writer
     6.3 pack index writer
```

### 3.3 File Scanner

职责：

- 扫描工作目录。
- 读取文件属性。
- 过滤忽略规则。
- 生成待提交文件列表。

设计：

- MVP 可全量扫描。
- P1 增加本地状态缓存，避免每次读取所有文件。
- 文件路径只进入本地流程，远端不保存明文路径。

技术：

- Rust std/fs 或 `walkdir`。

健壮性：

- 扫描期间文件变化时，应记录警告并重试该文件。
- 权限不足文件应跳过并报告，而不是中断整个 commit。
- 提交前必须执行路径策略校验，避免把目标平台永远无法检出的路径固化进不可变 tree。
- commit 必须定义源文件一致性策略，避免读取到不存在于任何时刻的 torn read。
- 对大文件、数据库文件、虚拟机镜像、模型权重等长时间读取对象，默认必须执行读前和读后的 metadata 校验，包括 size、mtime、平台可用的 change id 或 inode generation。
- 如果读后校验发现文件变化，必须丢弃该文件本次产生的临时 chunk/file object，并重试或报告文件不稳定。
- 支持平台级只读快照的环境应优先从快照视图读取，例如 Windows VSS、Linux LVM/btrfs/zfs snapshot。
- 如果无法获得平台快照，系统不得把活跃文件的一次长时间读取结果标记为强一致快照，只能按不稳定源处理。
- 对用户明确标记为 active database / VM image 的路径，MVP 要求停写或使用外部快照；不稳定读取结果不能作为强一致快照输入。

阶段：

- MVP。

WorkingTree 边界：

`WorkingTree` 负责所有与真实工作目录相关的副作用，核心对象模型不得直接访问文件系统。

接口职责：

- `scan`：执行 ignore、路径策略、文件 metadata 采集和源文件一致性策略。
- `open_stable_file`：按平台快照或读前/读后 metadata 校验提供稳定读输入。
- `plan_checkout`：在写入前完成路径 dry-run、冲突检测、磁盘空间和权限预检查。
- `write_checkout_temp`：只写临时路径，所有 chunk 认证和 object ID 校验完成后再原子发布。
- `record_platform_name_mapping`：记录 macOS/APFS/HFS+ 等平台的本地路径 read-back 映射。

规则：

- Commit、checkout、Web、VFS、SDK 不得各自实现路径策略；必须通过 `WorkingTree` 或 `ReadService` 使用同一套规则。
- `WorkingTree` 输出给 Object Model 的路径必须已经按当前 `PathPolicy` 规范化并通过校验。
- `WorkingTree` 只属于本地工作区边界，不进入远端事实源；本地 platform name mapping 可删除并通过重新 scan 恢复。

路径策略：

本系统是多设备同步系统，路径必须在进入 Object Model 前明确策略。默认使用跨平台安全策略：

```text
PathPolicy
  portable-strict
  platform-native
  escaped-checkout
```

MVP 默认：

```text
portable-strict
```

`portable-strict` 规则：

- 文件名必须是合法 UTF-8。
- Unicode 统一规范化为 NFC。
- 禁止 `/` 和 `\0`。
- 禁止 Windows 非法字符：`< > : " \ | ? *`。
- 禁止 Windows 保留名：`CON`, `PRN`, `AUX`, `NUL`, `COM1` 等。
- 禁止路径组件尾部空格和点。
- 检测大小写折叠冲突，例如同一目录下同时存在 `Readme` 和 `README`。
- checkout 前执行 dry-run path validation，避免写到一半失败。

平台规范化与检出幂等性：

- Object Model 内部统一保存 NFC 名称，但 checkout 不能假设目标文件系统会按字节保留 NFC。
- checkout 后必须执行平台路径 read-back 校验，确认写入名称、读取名称、重新扫描后的 normalized name 三者在路径策略下等价。
- macOS/APFS/HFS+ 必须单独测试 NFC/NFD、大小写折叠、组合音标和 emoji variation 等路径样例。
- 如果平台返回的名称与内部 NFC 名称规范化等价但字节形态不同，客户端必须在本地 checkout metadata 中记录映射，用于 scan/checkout 幂等判断。
- 该映射只属于本地工作区状态，不进入远端 manifest，不作为事实源。

阶段归属：

- MVP 实现 `portable-strict`。
- P1 实现 `platform-native`，允许平台原生路径，适合单平台仓库。
- P1 实现 `escaped-checkout`，跨平台检出时将不兼容名称映射为转义名称，并生成冲突报告。
- P2 实现 Linux 原生 bytes path 支持，并明确跨平台 checkout 映射规则。

### 3.4 Chunker

职责：

- 将连续字节流切成稳定 chunk。
- 提高插入、删除、局部修改后的复用率。

默认技术：

- `fastcdc`。

默认参数：

```text
min_chunk_size: 64 KiB
avg_chunk_size: 1 MiB
max_chunk_size: 8 MiB
```

Profile：

| Profile | 平均 chunk | 场景 | 阶段 |
| --- | --- | --- | --- |
| default | 1 MiB | 通用数据 | MVP |
| small-files | 256 KiB | 代码、文档、小图片 | P1 |
| large-binary | 4 MiB | 视频、模型、归档 | P1 |

Chunking Policy Engine：

CDC 不是所有文件格式的银弹。压缩包、加密归档、视频容器、某些模型权重或高熵二进制文件可能表现出较低增量复用率。系统必须把 chunker 选择建模为策略，而不是把 FastCDC 写死。

策略输入：

- 文件大小。
- 扩展名。
- 采样熵。
- 历史 dedup ratio。
- 用户指定 profile。
- 文件格式是否压缩、加密或容器化。

策略输出：

```text
ChunkingDecision {
  chunker_id,
  chunker_config_id,
  reason,
  expected_dedup_behavior,
}
```

MVP：

- 默认使用 FastCDC。
- 记录每个文件的 chunker 信息。
- 记录 commit 后的 dedup 统计。

P1：

- 增加 `large-binary` profile。
- 对已知高熵扩展名给出提示，例如 `.zip`, `.7z`, `.gz`, `.mp4`, `.mov`, `.pt`, `.safetensors`, `.onnx`。
- 支持用户显式指定 chunker/profile。

P2：

- format-aware chunker，例如 safetensors 按 tensor block、tar 按成员文件、parquet 按 row group。
- fixed-size block 作为策略引擎支持的 chunker 类型，用于用户显式选择或策略判定。

Dedup 统计：

```text
file_size
new_bytes
reused_bytes
dedup_ratio
chunker_id
chunker_config_id
```

当某类文件连续多次低去重时，客户端应提示用户切换 profile，并报告近似全量上传成本。

扩展契约：

```rust
pub trait Chunker: Send + Sync {
    fn id(&self) -> ChunkerId;
    fn config_fingerprint(&self) -> ChunkerConfigId;
    fn split<'a>(
        &self,
        reader: Box<dyn std::io::Read + Send + 'a>,
    ) -> Box<dyn Iterator<Item = Result<ChunkSpan>> + Send + 'a>;
}

pub struct ChunkSpan {
    pub offset: u64,
    pub len: u64,
}
```

File Object 必须记录：

```text
chunker_id
chunker_config_id
```

扩展规则：

- 同一个文件不同版本可以使用不同 chunker。
- chunker 参数变化必须改变 `chunker_config_id`。
- 不认识 chunker 的客户端不得重写该 file object。
- 语义级 chunker 属于 P2。

### 3.5 Hasher

职责：

- 计算仓库内稳定 object ID。
- 支撑去重。
- 避免远端通过公开文件猜测对象是否存在。

设计：

不能使用裸 `BLAKE3(plaintext)` 作为远端可见 ID。必须使用 keyed hash：

```text
object_id = BLAKE3_keyed(repo_object_id_key, canonical_plaintext)
```

效果：

- 同仓库内相同内容可去重。
- 不知道仓库密钥的人无法计算 object ID。
- 不支持跨仓库去重。

隐私边界：

- keyed object ID 防止跨仓库已知明文枚举，但不隐藏同一仓库内的内容重合度。
- 如果远端能观察 loose object 名称、pack index 或上传模式，它可以统计 object 复用频率和相等性。
- 对固定 header、空块、模板化二进制格式等数据，频率特征可能泄露数据类型或内容轮廓。
- CDC 去重会暴露同仓库内对象相等性；P3 引入 ORAM / 访问模式隐藏以降低该泄露。

技术：

- `blake3` keyed mode。

阶段：

- MVP。

### 3.6 Encryptor

职责：

- 将 chunk/file/tree/snapshot/ref 封装为加密对象。
- 提供完整性认证。
- 防止对象类型替换和跨仓库替换。

加密封包：

```text
EncryptedObject {
  magic,
  format_version,
  object_type,
  crypto_suite,
  key_epoch,
  padding_policy,
  object_id,
  nonce,
  ciphertext,
  auth_tag
}
```

AEAD Associated Data：

```text
magic
format_version
repo_id
object_type
object_id
crypto_suite
key_epoch
padding_policy
```

大小与流量侧信道：

- 加密只隐藏内容，不隐藏上传时序、对象数量、对象大小、pack 大小和访问模式。
- CDC chunk size 序列、manifest 大小、tree fanout 和 pack/index 访问模式可能泄露数据类型、目录规模或与公开数据集的相似性。
- MVP/P1 威胁模型不包含高级流量分析或国家级观察者；P3 通过 ORAM-style layout、padding 和 traffic shaping 扩展该威胁模型。
- Encryptor 必须支持可配置 padding policy；MVP 实现 `none` 和 manifest 随机 padding，P1 实现固定桶策略。

Padding Policy：

```text
PaddingPolicy
  none
  fixed-block-4k
  fixed-block-64k
  randomized-manifest-padding
  pack-level-padding
```

规则：

- padding 必须位于 AEAD 认证范围内，解密后由对象解码器剥离。
- object ID 仍基于 canonical plaintext，不包含随机 padding，避免同内容去重失效；因此 padding 只能隐藏大小，不能隐藏同仓库对象相等性。
- 对 manifest/tree/snapshot 等小对象，优先使用随机长度 padding 或固定桶对齐，降低目录结构大小泄露。
- 对 chunk 对象，padding 会增加存储和带宽成本，并可能削弱 range read 和 pack locality；MVP 对 chunk 使用 `none`，P1 实现固定桶策略。
- padding policy 是对象加密参数的一部分，必须进入对象 header 和 AEAD AD。

认证边界：

- 解密流程必须遵守先认证后释放明文的原则。
- 单个 chunk 对象必须作为最小认证单元；checkout、serve、VFS 不得把未通过 AEAD 验证的明文字节暴露给目标文件、HTTP 响应或调用方。
- P1 pack 和大对象内部流式加密必须采用分段认证结构，每个内部段独立携带认证标签和序号域分离。
- 验证失败时必须中止当前对象读取，并清理该对象已经写入的临时输出；失败对象不得进入可见 checkout 路径。
- checkout 应先写入临时路径，只有所有目标 chunk 认证和 object ID 校验完成后，才能原子发布最终文件。
- VFS 和 HTTP Range 读取只能返回已经完成认证的 chunk 或内部段。

技术：

- 首选 `XChaCha20-Poly1305`。
- P1 实现 `AES-256-GCM` 作为第二 suite。

nonce：

如果使用 AES-GCM，nonce 必须从 object ID 和用途域派生：

```text
nonce = first_96_bits(BLAKE3_keyed(repo_nonce_key, object_id || object_type || version))
```

阶段：

- MVP。

### 3.7 Stream Pipeline

职责：

- 在大文件下保持流式处理。
- 控制内存。
- 并行利用 CPU 和 I/O。

流水线：

```text
reader -> chunker -> hasher -> encryptor -> uploader
```

技术：

- `tokio` 负责 I/O、上传、任务编排。
- `rayon` 或 blocking pool 负责 hash/encrypt/chunking。

健壮性：

- 必须有背压。
- 必须限制 in-flight chunk 数量。
- 大文件提交时 peak memory 必须可配置。
- 失败 chunk 可重试，不重跑整个文件。

阶段：

- MVP 基础流水线。
- P1 性能调优。

### 3.8 Pack Writer

职责：

- 降低远端小对象数量。
- 降低请求成本。
- 支撑 range read、VFS 和大仓库 clone。

阶段设计：

```text
MVP:
  loose object

P1:
  append-only pack
  encrypted pack payload
  encrypted pack index

P2:
  repack
  hot/cold layout
  GC cooperation
```

pack 结构：

```text
pack:
  encrypted payload containing many objects

pack_index:
  object_id -> physical_object_ref, offset, length, layout_id
```

规则：

- `pack_id` 不是远端路径，必须通过 `StorageLayout` 解析为物理位置。
- pack index 只能暴露逻辑对象到 `PhysicalObjectRef` 的映射，不得要求上层知道 remote key、bucket、prefix 或 ORAM position。
- range read、VFS 和 clone 必须通过 `StorageLayout` + pack index 读取，不得直接拼接远端路径。

Pack Index 设计：

Pack Index 不能设计成单一全局可变文件，也不能要求每次 fetch 都拉取所有 per-pack index。必须采用不可变 index segment + 分层 compaction。

结构：

```text
packs/
  pack-<pack_id>.pack
  pack-<pack_id>.idx          // immutable per-pack index

pack-index/
  level0/
    index-<segment_id>.idx    // new small immutable index segment
  level1/
    index-<segment_id>.idx    // compacted segment
  level2/
    index-<segment_id>.idx
  roots/
    index-root-<generation>   // segment manifest
```

规则：

- 每个 pack 自带不可变 `.idx`，发布后不修改。
- 新 pack 发布新的 index segment，不修改全局大 index。
- index root 只保存 segment 清单和 generation。
- index root 必须记录 `layout_id` 和 layout generation。
- 支持 CAS 的后端可 CAS 更新 index root。
- 不支持 CAS 的后端只能在 single-writer 模式下执行 index root 更新和 compaction。
- 客户端必须维护本地 pack index cache，避免每次 fetch 全量拉取。

查询流程：

```text
read index root
  -> fetch missing index segments
  -> update local pack index cache
  -> lookup object_id -> physical_object_ref, offset, length, layout_id
  -> resolve physical_object_ref through StorageLayout
```

Compaction：

- P1 只追加 level0 index segment。
- P1 后期或 P2 增加手动 `pack-index compact`。
- compaction 生成新的高层 index segment，不原地修改旧 segment。
- compaction 成功发布新 root 后，旧 segment 由 GC 在安全窗口后清理。
- 弱后端 compaction 必须 single-writer 或禁用。
- compaction 不得改变逻辑 ObjectId，只能生成新的 physical_object_ref 和 layout generation。
- P3 历史强撤销和 ORAM layout 都复用 compaction/rewrite 的发布协议。

上传健壮性：

- pack 先写 `.partial`。
- 校验完成后发布 final pack。
- pack index segment 最后发布。
- index root 更新必须在 pack 和 index segment 都可读后执行。
- 未发布 index 的 partial pack 可清理。

## 4. 密码学平面架构

### 4.1 职责

密码学平面负责：

- 用户密码到仓库密钥的解锁。
- 仓库内对象 ID key 的派生。
- 对象加密 key 的派生。
- keyring 管理。
- 多设备和团队共享的密钥封包。

### 4.2 模块树

```text
Crypto Plane
  1. KDF
  2. Repo Master Key
  3. Key Derivation
  4. Object Encryption
  5. Keyring
  6. Device Envelope
  7. Crypto Suite Migration
```

### 4.3 KDF

职责：

- 从用户密码派生 unlock key。

技术：

- `Argon2id`。

设计：

```text
user_password
  -> Argon2id(salt, params)
    -> unlock_key
```

KDF 参数写入 keyring header。

阶段：

- MVP。

### 4.4 Repo Master Key、Epoch 与子密钥

职责：

- 用一个随机仓库主密钥派生所有用途子密钥。

direct/pack layout 的物理结构示例：

```text
repo_master_key(epoch)
  -> repo_object_id_key
  -> repo_manifest_id_key
  -> repo_chunk_enc_key
  -> repo_manifest_enc_key
  -> repo_ref_key
  -> repo_nonce_key
  -> repo_path_index_key
```

设计：

- 用户密码只解锁当前可访问 epoch 的 `repo_master_key`。
- 用户密码不直接加密数据对象。
- 子密钥必须带用途域分离。
- 对象必须记录 `key_epoch`，用于选择对应 epoch 的子密钥解密。
- 仓库配置必须记录 `active_epoch`，新对象只能使用 active epoch 加密。
- P2 撤销必须轮转 `repo_object_id_key`、manifest id、加密、nonce、ref、path index 相关子密钥，避免被撤销设备计算未来对象 ID、目录分片 key 或路径 token。
- 旧 epoch 只用于读取旧对象，不得用于写入新对象。

阶段：

- MVP。
- P2：设备撤销时引入 epoch 轮转。

### 4.5 Keyring

职责：

- 保存被 unlock key 或设备公钥加密的 `repo_master_key`。

结构：

```text
Keyring {
  format_version,
  repo_id,
  active_epoch,
  kdf_params,
  crypto_suite,
  envelopes: [KeyEnvelope]
}
```

MVP：

- 单用户密码 envelope。

P2：

- 多设备 envelope。
- 设备授权。
- 设备撤销。
- 小团队共享。

健壮性：

- keyring 更新必须走本地锁和 journal。
- keyring 损坏会导致仓库不可解锁，必须支持本地备份提示。
- keyring 永远不得 in-place overwrite。
- keyring 更新必须使用 generation + atomic publish 协议。
- 至少保留最近 N 代 keyring，避免单次写坏导致仓库永久不可解锁。

Keyring 原子更新协议：

本地 keyring 更新必须遵循：

```text
write keyring.<generation>.tmp
  -> fsync(tmp file)
  -> read-back validate
  -> atomic rename tmp -> keyring.<generation>
  -> fsync(parent directory)
  -> write keyring.current.tmp
  -> fsync(keyring.current.tmp)
  -> atomic rename keyring.current.tmp -> keyring.current
  -> fsync(parent directory)
```

禁止：

- 覆盖写 `.e2v/keyring`。
- 截断原 keyring 后重写。
- 在没有 fsync 的情况下认为 keyring 更新成功。

远端 keyring 发布协议：

```text
upload keys/keyring.<generation>
  -> verify upload
  -> publish keyring.current with CAS
  -> retain previous generations
```

保留策略：

```text
min_retained_generations: 5
min_retention_days: 30
```

密码修改：

- 修改密码只新增 keyring generation。
- grace period 内保留旧 generation。
- 用户可显式执行 keyring prune。

设备撤销冲突：

- 保留旧 keyring 有利于灾难恢复，但可能削弱撤销语义。
- MVP 不支持团队撤销。
- P2 设备撤销必须同时移除该设备 envelope 并推进 `active_epoch`，新对象使用新 epoch 的 `repo_master_key` 派生密钥加密。
- 已获得旧 epoch key 的设备仍可读取旧对象；若远端存储访问凭证未吊销，且没有 epoch 轮转，它也能读取未来对象，因此禁止把“仅删除 envelope”称为有效撤销。
- P3 实现历史数据强撤销，必须通过全仓库重加密、旧 epoch 退休和对象重写 journal 完成。
- P3 历史强撤销不得原地覆盖旧对象，必须生成新 physical refs、新 layout generation，再原子发布 layout root 和 ref。
- P3 历史强撤销完成后，旧 epoch physical refs 只能在 GC 确认没有可读 layout generation 引用后删除。

回滚防护：

- 客户端应记录本地见过的最高 keyring generation。
- 远端返回更低 generation 时必须警告或拒绝，防止恶意回滚。

### 4.6 多设备和团队共享

阶段：

- P2。

原则：

- 先多设备，后团队。
- 设备撤销必须包含 keyring envelope 移除、`active_epoch` 推进和未来对象改用新 epoch 加密。
- P2 设备撤销阻止未来数据访问。
- P3 历史强撤销通过重加密历史数据移除旧 epoch 访问能力。
- P3 历史强撤销复用 `StorageLayout` rewrite 协议，不改变 Snapshot DAG 的逻辑历史结构。
- 存储后端访问凭证撤销属于外部权限面，必须在产品文档中单独提示；密钥轮转不能阻止已持有远端读权限的设备下载旧密文。

技术：

- X25519/HPKE 风格 envelope。
- 不建议直接背上完整 PGP 复杂度。

### 4.7 Crypto Suite 边界

职责：

- 明确对象使用的加密算法套件。
- 避免同一仓库内对象缺少可验证的算法参数。

设计：

- 每个对象带 `crypto_suite`。
- 仓库配置记录默认 `crypto_suite`。
- 当前阶段只定义默认 suite 和对象 header 标记。
- 读取对象时必须按对象 header 的 `crypto_suite` 选择算法；不认识的 suite 直接拒绝处理。

阶段：

- MVP 写入 `crypto_suite` 字段。

## 5. 元数据与版本平面架构

### 5.1 职责

元数据与版本平面负责：

- 描述文件、目录、快照和分支。
- 维护 Merkle DAG。
- 支撑历史查询、分支、稀疏检出和本地搜索。

### 5.2 模块树

```text
Metadata & Version Plane
  1. Object Model
     1.1 chunk
     1.2 file
     1.3 tree
     1.4 directory_root
     1.5 tree_shard
     1.6 snapshot
     1.7 ref
  2. Manifest Store
     2.1 object decoder
     2.2 batch reader
     2.3 tree walker
     2.4 manifest cache
  3. Merkle DAG
  4. Snapshot Manager
  5. Branch / Ref Manager
  6. Local Index
  7. Read Service
  8. Sparse Checkout
```

### 5.3 Object Model

对象类型：

| 对象 | 内容 | 是否加密 | 阶段 |
| --- | --- | --- | --- |
| `chunk` | 文件内容分块 | 是 | MVP |
| `file` | chunk 列表、文件属性、chunker 信息 | 是 | MVP |
| `tree` | 目录项、文件名、子目录指针 | 是 | MVP |
| `directory_root` | 大目录分片根、分片 fanout、排序策略 | 是 | MVP/P1 |
| `tree_shard` | 大目录分片项 | 是 | MVP/P1 |
| `snapshot` | root tree、父快照、提交信息 | 是 | MVP |
| `ref` | 分支名 token 到 snapshot 指针 | 是 | MVP/P1 |
| `pack` | 多对象物理打包 | 是 | P1 |
| `pack_index` | pack 偏移索引 | 是或半公开 | P1 |

必须使用 canonical encoding，不能依赖不稳定 JSON 字段顺序。

格式边界：

- 当前阶段只定义对象类型、必需字段和 canonical encoding。
- 新增对象字段必须先进入 Object Model 的显式 schema，并纳入 canonical encoding、object ID 和 AEAD 认证范围。
- 不允许隐式忽略 manifest 字段；解码器遇到当前 schema 不认识的对象格式时必须拒绝处理。

编码决策：

- MVP 使用 `postcard` 加显式版本约束。
- P1 如需跨语言稳定规范，再引入 canonical CBOR 或自定义 canonical binary format。

路径名称：

- MVP 的 tree entry 只接受 `portable-strict` 路径策略下的 normalized UTF-8 名称。
- tree plaintext 中保存 normalized entry name；整个 tree object 作为 manifest 加密对象写入远端，因此远端不可见明文文件名和目录项。
- path policy 必须写入仓库配置或 snapshot metadata，避免不同客户端对同一 tree 使用不同解释。
- P2 实现 raw bytes path，并必须明确跨平台 checkout 映射规则。

Manifest 大小上限：

任何单个 manifest 对象都必须有大小上限，避免巨型目录导致内存峰值失控。

默认上限：

```text
max_manifest_plaintext_size: 4 MiB
max_manifest_encrypted_size: 8 MiB
max_tree_entries_per_object: 4096
```

超过上限时必须使用 Tree Sharding，不允许构建超大单体 tree object。

Tree Sharding 阶段边界：

- 如果 MVP 明确支持十万级小文件或百万级目录压力测试，`directory_root` 和 `tree_shard` 必须在 P0-A 实现。
- 如果 MVP 只验证小规模本地快照，可先只实现单体 `tree`，但 P0 schema 必须保留 `directory_root`、`tree_shard` 对象类型编号和拒绝未知格式的升级路径。
- P1 必须实现 Tree Sharding；实现后不能改变已有单体 `tree` 的 object ID 语义，只能让新快照在超过阈值时写入分片结构。

### 5.4 Merkle DAG

结构：

```text
snapshot
  -> root tree
       -> file object
            -> chunk object list
       -> child tree
            -> file object
```

规则：

- chunk/file/tree/snapshot 不可变。
- ref 是唯一可变入口。
- branch 只是一条 ref。

### 5.5 Manifest Store

职责：

- 解码 snapshot/tree/file。
- 支持批量读取。
- 支持目录遍历。
- 避免 N+1 查询。

接口：

```rust
#[async_trait]
pub trait ManifestStore: Send + Sync {
    async fn get_snapshot(&self, id: &ObjectId) -> Result<SnapshotObject>;
    async fn get_tree(&self, id: &ObjectId) -> Result<TreeObject>;
    async fn get_file(&self, id: &ObjectId) -> Result<FileObject>;
    async fn get_many(&self, ids: &[ObjectId]) -> Result<Vec<ManifestObject>>;
    async fn walk_tree(
        &self,
        root: &ObjectId,
        options: WalkOptions,
    ) -> Result<BoxStream<'static, Result<TreeEntry>>>;
}
```

N+1 控制：

- 批量读取 manifest。
- tree entry 分页。
- 目录遍历 stream。
- manifest LRU cache。
- ref cache 不得永久缓存。

Tree Sharding：

为了支持单目录十万级甚至百万级文件，目录对象必须支持分片。分片算法必须 deterministic，确保不同客户端对相同目录生成相同 tree。

推荐结构：

```text
DirectoryRoot
  -> TreeShard(prefix=00)
  -> TreeShard(prefix=01)
  -> ...
  -> TreeShard(prefix=ff)
```

分片 key：

```text
shard_key = BLAKE3_keyed(repo_manifest_id_key, normalized_entry_name)
```

规则：

- 单个 shard entry 数超过阈值时继续增加 fanout。
- 单个 TreeShard 不得超过 `max_tree_entries_per_object`。
- 单个 TreeShard 不得超过 `max_manifest_plaintext_size`。
- tree walk 必须以 stream 形式返回，不得一次性展开整个目录。
- checkout 必须先执行 path validation dry-run，再开始写入文件。

构建策略：

```text
scan directory
  -> normalize and validate names
  -> sort by canonical name or shard key
  -> stream entries into bounded shard builder
  -> emit TreeShard objects
  -> emit DirectoryRoot object
```

阶段：

- P0-A：ManifestStore 必须提供 streaming tree walk API；小规模 MVP 可先写单体 `tree`，但 schema 必须保留 `directory_root` 和 `tree_shard` 对象类型编号。
- P0-A：如果 MVP 目标包含十万级小文件或百万级目录压力测试，Tree Sharding 必须前置实现。
- P1：如果 P0 未实现 Tree Sharding，P1 必须补齐；同时加入批量读取、分页、manifest cache 和目录遍历性能优化。

### 5.6 Snapshot Manager

职责：

- 创建 snapshot。
- 维护 parent 链。
- 读取历史。
- 支撑 checkout。

提交顺序：

```text
chunks -> files -> trees -> snapshot -> ref
```

健壮性：

- snapshot 写入成功但 ref 更新失败时，不丢弃 snapshot。
- ref 更新失败产生显式冲突。
- CAS 失败后，本地必须把该 snapshot 标记为 `unpublished` 或 `needs-rebase`，不得把它当成已经被远端 ref 保护的普通 head。
- 基于 `unpublished` snapshot 再次 push 前，必须验证该 snapshot 及其 parent chain 在目标远端完整存在，或先重新上传缺失 ancestor。
- 发起 push 前，不得只信任 Local Index 里的 parent 状态；将要作为 parent 的 snapshot 必须在远端执行 exists/read-back 校验。

阶段：

- MVP。

### 5.7 Branch / Ref Manager

职责：

- 管理分支名。
- 管理 ref token。
- CAS 更新 ref。
- 检测分叉。

ref token：

```text
ref_token = BLAKE3_keyed(repo_ref_key, "branch:" || branch_name)
```

规则：

- 分支名默认不明文暴露。
- ref 内容加密。
- ref 更新必须 CAS。
- 不支持 CAS 的后端不支持 multi-writer，只能进入 single-writer 模式。
- 先读后写的“冲突检测”不能替代原子 CAS。

阶段：

- MVP/P0-B 基础 ref。
- P1 分支命令。

### 5.8 Local Index

职责：

- 提供本地搜索和浏览加速。
- 缓存展开后的 manifest。
- 记录上传/下载状态。

不是事实源。删除后必须能从 manifest 重建。

技术：

- P1：SQLite。
- P1：SQLite FTS5。
- P2：RocksDB，用于极大规模 key-value cache。

索引内容：

- snapshot metadata cache。
- path 到 object 的映射。
- 文件名索引。
- 扩展名、大小、mtime。
- tree 展开缓存。
- pack index cache。

搜索分层：

| 搜索类型 | 阶段 | 默认 | 隐私影响 |
| --- | --- | --- | --- |
| metadata search | P1 | 是 | 本地索引可见文件属性 |
| filename FTS | P1 | 是 | 本地索引可见文件名 |
| content FTS | P2 | 实现能力，仓库需用户显式开启 | 本地保存明文内容索引 |
| encrypted remote search | 排除 | 不实现 | 不进入路线图 |

性能目标：

```text
100,000 files
500 snapshots
metadata query P95 < 100 ms
filename search P95 < 300 ms
index rebuild throughput > 20,000 entries/s
```

### 5.9 Read Service

职责：

- 为 CLI checkout、Web、Local HTTP API、VFS 和 SDK 提供统一只读视图。
- 隔离 ManifestStore、StorageLayout、LogicalObjectStore、cache 和认证边界。
- 避免每个访问入口重复实现目录遍历、文件打开和 range read。

接口：

```rust
#[async_trait]
pub trait ReadService: Send + Sync {
    async fn open_snapshot(&self, id: &ObjectId) -> Result<SnapshotHandle>;
    async fn resolve_branch(&self, token: &RefToken) -> Result<SnapshotHandle>;
    async fn read_dir(
        &self,
        snapshot: &SnapshotHandle,
        path: &SnapshotPath,
        options: ReadDirOptions,
    ) -> Result<DirPage>;
    async fn open_file(
        &self,
        snapshot: &SnapshotHandle,
        path: &SnapshotPath,
    ) -> Result<FileHandle>;
    async fn read_range(
        &self,
        file: &FileHandle,
        range: ByteRange,
    ) -> Result<Bytes>;
}
```

规则：

- `SnapshotHandle` 必须绑定 snapshot id 和 layout generation，避免读取过程中混用不同 layout。
- `FileHandle` 必须绑定 file object id、chunk list、crypto suite、key epoch 和 layout generation。
- `read_range` 必须只返回已经完成 AEAD 认证和 object ID 校验的字节。
- Web、HTTP API、VFS、SDK 不得直接解密 chunk 或直接访问 `BlobStore`。
- snapshot-pinned 读取默认稳定；live branch 读取必须显式重新 resolve branch，不能静默改变已有 handle。

阶段：

- P0-A：支持 snapshot browse、directory browse、open file、read range 的内部 API。
- P0-C：Web 和 Local HTTP API 使用同一 `ReadService`。
- P2：VFS 只读实现只依赖 `ReadService`，不绕过到 ManifestStore 或 StorageLayout。

### 5.10 Sparse Checkout

职责：

- 只拉取指定路径、指定深度的数据。

MVP 策略：

```text
root tree
  -> selected child tree
      -> selected file
          -> selected chunks
```

P1 路径 token：

```text
path_token = BLAKE3_keyed(repo_path_index_key, normalized_path)
```

权衡：

- 路径 token 加速定位。
- 会泄露相同路径 token 的相等性。
- P1 实现路径 token 索引；仓库需用户显式开启，启用后必须在仓库配置中记录该隐私降级。

## 6. 存储与同步平面架构

### 6.1 职责

存储与同步平面负责：

- 抽象本地和远端存储。
- 处理不可变对象读写。
- 处理可变 ref 更新。
- 实现 push/fetch/clone。
- 实现断点续传、崩溃恢复、GC 和防腐。

### 6.2 模块树

```text
Storage & Sync Plane
  1. StorageLayout
  2. LogicalObjectStore
  3. BlobStore
  4. RefStore
  5. LayoutRootStore
  6. TransactionPublisher
  7. Backend Capability
  8. Backend Adapter
     8.1 Local Folder
     8.2 S3-compatible
     8.3 WebDAV/Alist
     8.4 Memory
  9. Sync Engine
     9.1 push
     9.2 fetch
     9.3 clone
     9.4 pull
  10. Operation Journal
  11. Upload Resume
  12. GC / Verify / Repair
```

### 6.3 StorageLayout

职责：

- 隔离逻辑 `ObjectId` 与远端物理存储位置。
- 支撑 direct layout、pack layout、rewrite layout 和 P3 ORAM layout。
- 给 GC、verify、repair、VFS、clone 提供统一的对象定位和枚举入口。

结构：

```text
StorageLayout
  layout_id
  layout_generation
  logical_object_id -> PhysicalObjectRef

PhysicalObjectRef
  layout_id
  container_id
  offset_optional
  length
  access_hint
```

规则：

- `ObjectId` 不得直接作为远端路径使用。
- P0 direct layout 可以把 `ObjectId` 映射为简单 loose object 物理引用，但该映射必须封装在 `StorageLayout` 内。
- P1 pack layout 把 `ObjectId` 映射到 pack container + offset + length。
- P3 rewrite/oram layout 可以改变 physical object ref、访问调度和远端位置，但不得改变 Object Model、Manifest Store 和 Snapshot DAG。
- P3 ORAM layout 必须把访问计划、padding schedule、reshuffle generation 作为 layout metadata 管理。
- ORAM layout 不得要求 `BlobStore` 理解 `ObjectId`，也不得要求 GC 直接解释 ORAM bucket 内容。
- GC 只能通过 `StorageLayout` 枚举 physical refs 与 logical refs 的关系，不得直接扫描远端 prefix 后推断对象可达性。
- VFS range read 只能请求 logical file/chunk range，由 layout 解析到物理读取计划。
- layout root 更新必须具备 generation 和 CAS/lease 发布协议，避免 rewrite 或 ORAM 更新与普通 push 互相覆盖。

阶段：

- P0：direct layout。
- P1：pack layout。
- P3：rewrite layout 和 ORAM layout。

### 6.4 LogicalObjectStore

职责：

- 提供逻辑对象读写入口，封装 canonical encode、object ID、加密、解密、认证和 layout 解析。
- 让 ManifestStore、Snapshot Manager、Sync Engine、ReadService 不直接接触物理对象路径。
- 在 P0 direct layout、P1 pack layout、P3 rewrite/ORAM layout 之间保持上层 API 稳定。

接口：

```rust
#[async_trait]
pub trait LogicalObjectStore: Send + Sync {
    async fn put_object(
        &self,
        object_type: ObjectType,
        plaintext: CanonicalObjectBytes,
        options: PutObjectOptions,
    ) -> Result<StoredObject>;
    async fn get_object(&self, id: &ObjectId, expected_type: ObjectType) -> Result<PlainObject>;
    async fn get_object_range(
        &self,
        id: &ObjectId,
        expected_type: ObjectType,
        range: ByteRange,
    ) -> Result<Bytes>;
    async fn exists_object(&self, id: &ObjectId) -> Result<bool>;
    async fn resolve_object(&self, id: &ObjectId) -> Result<PhysicalReadPlan>;
}
```

规则：

- `put_object` 负责生成 keyed `ObjectId`、选择 active epoch、写入 AEAD header，并通过 `StorageLayout` 生成 physical ref。
- `get_object` 必须先通过 AEAD 认证，再释放 plaintext 给上层解码器。
- `get_object_range` 只能用于支持独立认证边界的对象或 chunk/pack 内部段；不得返回未认证片段。
- `exists_object` 是逻辑对象存在性检查，不等价于某个 physical path 存在。
- ManifestStore 只能依赖 `LogicalObjectStore` 读取 manifest 对象，不得依赖 `BlobStore`。

阶段：

- P0：direct layout + loose encrypted object。
- P1：pack layout 和 pack index 作为实现细节接入。
- P3：rewrite/ORAM layout 只替换 physical read/write plan。

### 6.5 BlobStore

职责：

- 读写物理不可变对象。

接口：

```rust
#[async_trait]
pub trait BlobStore: Send + Sync {
    async fn put_physical(&self, reference: &PhysicalObjectRef, bytes: Bytes) -> Result<PutResult>;
    async fn get_physical(&self, reference: &PhysicalObjectRef) -> Result<Bytes>;
    async fn get_physical_range(&self, reference: &PhysicalObjectRef, range: ByteRange) -> Result<Bytes>;
    async fn exists_physical(&self, reference: &PhysicalObjectRef) -> Result<bool>;
    async fn stat_physical(&self, reference: &PhysicalObjectRef) -> Result<ObjectStat>;
    async fn list_physical(
        &self,
        scope: ListScope,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<ListPage>;
}
```

规则：

- `BlobStore` 只处理物理对象，不理解 `ObjectId`、snapshot、tree 或 file 语义。
- 逻辑对象读写必须先经过 `StorageLayout` 解析。
- 后端 adapter 可以有 prefix/list 能力，但上层不得把 prefix 结构当作对象模型的一部分。

阶段：

- MVP。

### 6.6 RefStore

职责：

- 读写可变 ref。
- 提供 CAS。

接口：

```rust
#[async_trait]
pub trait RefStore: Send + Sync {
    async fn read_ref(&self, token: &RefToken) -> Result<Option<EncryptedRef>>;
    async fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<RefVersion>,
        next: EncryptedRef,
    ) -> Result<CasResult>;
}
```

规则：

- ref 是唯一可变入口。
- CAS 失败必须显式冲突。
- 不允许最后写入者静默覆盖。

### 6.7 LayoutRootStore

职责：

- 管理当前可读 layout root、layout generation 和历史 layout root 保留窗口。
- 为 pack compaction、layout rewrite、历史强撤销和 ORAM reshuffle 提供统一发布入口。

接口：

```rust
#[async_trait]
pub trait LayoutRootStore: Send + Sync {
    async fn read_layout_root(&self) -> Result<LayoutRoot>;
    async fn compare_and_swap_layout_root(
        &self,
        expected: LayoutRootVersion,
        next: LayoutRoot,
    ) -> Result<CasResult>;
    async fn list_retained_layout_roots(&self) -> Result<Vec<LayoutRoot>>;
}
```

规则：

- direct layout 也必须有 layout root，即使内容只有 `layout_id`、generation 和 direct mapping policy。
- ref 不得指向 layout root 无法解析的 object；发布顺序必须由 `TransactionPublisher` 保证。
- GC 必须同时保留 current layout root 和 grace period 内的 previous layout roots。
- 不支持 layout root CAS/lease 的后端不得执行 pack index compaction、layout rewrite 或 ORAM 更新。

阶段：

- P0-A：本地 direct layout 必须有 layout root 和 generation，供 ReadService、checkout 和 verify 绑定读取视图。
- P0-B：定义远端 layout root CAS/lease 发布边界。
- P1：pack index root 通过同一发布边界接入。
- P3：rewrite/ORAM root 复用同一发布边界。

### 6.8 TransactionPublisher

职责：

- 统一编排 writer lease、remote write intent、layout root 发布、ref CAS、journal recovery 和 pre-commit verify。
- 把远端发布协议从 CLI、Sync Engine 和 backend adapter 中抽离出来。

接口：

```rust
#[async_trait]
pub trait TransactionPublisher: Send + Sync {
    async fn begin(&self, plan: PublishPlan) -> Result<PublishSession>;
    async fn record_uploaded(&self, session: &PublishSession, object: PublishedObject) -> Result<()>;
    async fn publish_layout_if_needed(&self, session: &PublishSession) -> Result<LayoutRootVersion>;
    async fn pre_commit_verify(&self, session: &PublishSession) -> Result<()>;
    async fn publish_ref(&self, session: &PublishSession, next: EncryptedRef) -> Result<CasResult>;
    async fn complete(&self, session: PublishSession) -> Result<()>;
    async fn recover(&self, operation_id: &OperationId) -> Result<RecoveryAction>;
}
```

规则：

- `begin` 必须根据 backend capability 选择 multi-writer、single-writer 或 read-only 模式。
- `publish_ref` 前必须确认 write intent 和 writer lease 新鲜、layout root 可解析、ancestor closure 完整、目标 ref 仍等于 expected head。
- Operation Journal 中的 `uploaded` 只能作为恢复线索，不能替代 `pre_commit_verify`。
- Sync Engine 只提交 `PublishPlan`，不得自行更新 ref 或 layout root。
- GC、repair、pack compaction 和历史强撤销必须复用同一 transaction/recovery 模型。

阶段：

- P0-B：push/fetch/clone 发布协议和 upload resume。
- P1：pack index root、GC execute 和 repair 接入。
- P3：layout rewrite、历史强撤销和 ORAM reshuffle 接入。

### 6.9 Backend Capability

职责：

- 描述后端能力。
- 决定是否启用 VFS、GC、pack、range read 等功能。

结构：

```rust
pub struct BackendCapability {
    pub supports_conditional_put: bool,
    pub supports_range_read: bool,
    pub supports_atomic_rename: bool,
    pub supports_paged_list: bool,
    pub has_strong_consistency: bool,
    pub supports_remote_lock_or_lease: bool,
    pub supports_transaction_markers: bool,
    pub supports_reliable_remote_time: bool,
    pub supports_object_generation_or_etag: bool,
    pub supports_layout_root_cas: bool,
    pub supports_oblivious_access_schedule: bool,
}
```

降级规则：

| 缺失能力 | 降级 |
| --- | --- |
| no range read | 禁用 VFS 和大文件流式预览 |
| no CAS | 禁用 multi-writer；只能使用带远端 lease 的 single-writer，或进入显式人工风险模式 |
| weak list | 禁用 `gc --execute`，只允许保守 verify 和 dry-run |
| no atomic rename | pack 使用 publish marker 和校验 |
| no remote lock/lease | 禁用 destructive GC 或要求离线单写者维护窗口 |
| no reliable remote time/version | 禁用 `gc --execute` 和无人值守 single-writer push |
| no layout root CAS/lease | 禁用 layout rewrite、pack compaction 发布和 P3 ORAM layout 更新 |
| no oblivious access support | 禁用 P3 ORAM layout |

写者模式：

```text
WriterMode
  multi-writer     // requires conditional put/CAS for ref
  single-writer    // no concurrent writers allowed
  read-only        // fetch/verify/checkout only
```

规则：

- `supports_conditional_put == false` 的后端不得声明支持 multi-writer。
- 对无 CAS 后端，客户端不得声称“冲突检测可以避免覆盖”。先读后写不是原子 CAS。
- single-writer 后端执行 push 前必须先获取远端 writer lease；只有后端提供可靠 lock/lease 或可验证的独占写入约束时，才允许常规 push。
- single-writer lease 必须包含 writer_id、operation_id、target_ref、remote_observed_at、lease_generation 和 heartbeat 对象。
- lease 获取、续期和释放必须基于远端可观察状态；本地 writer identity 只能作为诊断辅助，不能替代远端 lease。
- 检测到多个 writer identity 时，默认拒绝 push。
- 无 CAS 且无可靠远端 lease 的后端默认禁止后台自动 push；用户显式 `--force-single-writer-risk` 只能用于人工恢复或明确单写者维护窗口。
- WebDAV/Alist 默认按 single-writer 后端处理，除非适配器证明其具有可靠 CAS/lock 能力。

### 6.10 Backend Adapter

后端优先级：

1. Local folder：最容易调试协议。
2. S3-compatible：能力较稳定，适合首个远端。
3. WebDAV/Alist：能力参差不齐，放在 S3 后。
4. Memory：测试用。

技术：

- `opendal` 作为后端接入基础。
- 但核心语义仍由 `LogicalObjectStore` / `StorageLayout` / `BlobStore` / `RefStore` / `LayoutRootStore` 定义。

适配器准入测试：

- put/get。
- physical put/get/list。
- physical range read。
- layout root CAS/lease。
- paged list。
- conditional ref update。
- remote lock/lease acquire and heartbeat。
- remote Last-Modified / generation / ETag reliability。
- large object upload。
- Unicode path handling。
- retry on transient failure。

### 6.11 Sync Engine

Push：

```text
build PublishPlan
  -> TransactionPublisher.begin
  -> read remote ref
  -> compare expected head
  -> upload missing objects
  -> TransactionPublisher.record_uploaded
  -> write snapshot
  -> TransactionPublisher.publish_layout_if_needed
  -> TransactionPublisher.pre_commit_verify
  -> TransactionPublisher.publish_ref
  -> TransactionPublisher.complete
```

Push 规则：

- multi-writer push 必须依赖后端 CAS/conditional put，并由 `TransactionPublisher` 执行最终发布。
- single-writer 后端不得允许两个设备同时 push，writer lease 必须由 `TransactionPublisher` 获取、续期和释放。
- `write intent` 是远端可见的上传意图，用于告知 GC 有未发布对象正在写入。
- 大型 push 必须定期刷新 write intent heartbeat。
- push 崩溃后，write intent 在超时前会阻止 destructive GC。
- 执行最终 ref CAS 前必须做 pre-commit lease and intent validation。
- pre-commit validation 必须确认自己的 write intent 仍未过期、writer lease 仍归当前 operation 持有、远端 ref 仍等于 expected head。
- 如果 intent 或 writer lease 已过期，不得直接 CAS 更新 ref；`TransactionPublisher` 必须先续期 intent/lease，并对本次 snapshot 可达对象执行增量 `verify remote`。
- 增量 verify 必须确认 snapshot、tree、file、chunk 或 pack index 引用的所有对象仍在远端可读；确认后才能重新进入最终 CAS。
- pre-commit validation 还必须验证 snapshot parent chain 的远端 ancestor closure，不得发布 parent 在远端缺失的 snapshot。
- CAS 冲突失败后，本地操作结果必须进入 `needs-rebase` 状态；再次 push 该快照或其子快照前，必须重新检查 parent chain 并补传缺失 ancestor。
- 如果本次 push 产生新的 physical refs 或 pack refs，`publish_ref` 前必须确认 layout root 已发布且 layout generation 未被其他 writer 覆盖。
- ref CAS 和 layout root 发布必须由 `TransactionPublisher` 形成可恢复的两阶段协议：对象可先上传，layout 可先发布，但 ref 不得指向 layout 不可解析的对象。
- Operation Journal 中的 `uploaded` 状态只能表示“曾经上传成功”，不能作为最终 ref 发布前对象仍存活的证明。

远端 write intent：

```text
transactions/
  active/
    <operation_id>.intent
  completed/
    <operation_id>.done
```

intent 内容：

```text
RemoteWriteIntent {
  operation_id,
  writer_id,
  started_at_remote,
  heartbeat_remote_observed_at,
  expected_ref,
  target_branch_token,
  planned_snapshot_optional,
  client_version,
}
```

时间语义：

- intent 和 lease 的过期判断不得使用客户端本地时钟作为事实源。
- 支持 object metadata 的后端必须使用远端 `Last-Modified`、ETag/generation 或等价的远端可观察时间/版本来判断 heartbeat 是否新鲜。
- 不提供可靠远端时间、版本或强一致 list 的后端不得启用 destructive GC，也不得启用无人值守 single-writer push。
- 客户端本地时间只用于日志展示、退避和 UX 提示，不参与 GC 删除资格或 ref 发布安全判断。
- 如果必须使用客户端时间参与判定，push/gc 初始化阶段必须执行远端时间探测并设置严格 skew 上限；超过上限时拒绝危险操作。

隐私：

- intent 不记录明文路径。
- branch 使用 ref token。
- object 列表默认不写入远端 intent，避免泄露上传规模之外的更多结构。

Fetch：

- 拉取远端 ref 和必要 manifest。
- 不直接修改工作区。

Pull：

- fetch + fast-forward 判断。
- 分叉时标记 diverged。
- MVP 不自动 merge。

Clone：

```text
read public.json
  -> read keyring
  -> unlock repo_master_key
  -> read target ref
  -> decrypt snapshot
  -> pull tree/file/chunk on demand
  -> rebuild local index
```

### 6.12 Operation Journal

职责：

- 支持断点续传。
- 支持崩溃恢复。
- 记录操作阶段。

位置：

```text
.e2v/journal/
  operations.sqlite        // preferred for object-scale state
  wal/<operation_id>.log   // append-only alternative
```

原则：

- Operation Journal 不得是包含百万对象列表的单一 JSON 文件。
- 涉及对象列表的状态流转必须使用 SQLite 表或 append-only WAL。
- 小 JSON 只可用于 operation metadata，不可用于频繁更新的 object state。
- 每个对象上传完成时只能追加记录或更新一行，不得重写整个 journal。

元数据：

```text
OperationJournal {
  operation_id,
  operation_type,
  started_at,
  repo_id,
  branch,
  expected_head,
  target_snapshot,
  ref_update_state,
  last_error,
}
```

对象状态表：

```text
object_uploads(
  operation_id,
  object_id,
  object_type,
  state,        // planned, uploaded, verified, failed
  updated_at,
  retry_count,
  last_error
)

pack_uploads(
  operation_id,
  pack_id,
  state,        // planned, partial_uploaded, published, indexed, failed
  updated_at,
  retry_count,
  last_error
)

layout_rewrites(
  operation_id,
  layout_id,
  from_generation,
  to_generation,
  state,        // planned, rewriting, published, failed
  updated_at,
  retry_count,
  last_error
)
```

Append-only WAL 备选格式：

```text
BEGIN operation_id
PLAN_OBJECT object_id object_type
UPLOADED object_id
VERIFIED object_id
FAILED object_id error_code
SNAPSHOT_WRITTEN snapshot_id
REF_UPDATED ref_token generation
END operation_id
```

WAL 规则：

- 只追加。
- 每条记录带 checksum。
- 恢复时顺序 replay。
- 定期 compact 到 SQLite 或 checkpoint 文件。
- 不得一次性把全部对象状态读入内存。

隐私：

- 不记录明文路径。
- 不记录密钥。
- 必要路径信息使用本地 token 或本地加密保存。

阶段：

- P0-B。

### 6.13 Upload Resume

恢复流程：

```text
open operation journal
  -> page through object_uploads where state != verified
  -> stat uploaded objects
  -> exists planned objects
  -> upload missing objects in bounded batches
  -> renew write intent and writer lease if needed
  -> pre-commit incremental verify of reachable objects
  -> verify snapshot/ref
  -> continue CAS
```

恢复规则：

- 每次最多加载一个 bounded batch 的 object state。
- 恢复过程必须可重复执行。
- 对象状态从 `planned` 到 `uploaded` 到 `verified` 单向推进。
- 对状态不确定的对象执行 `exists/stat/read-back` 后再推进。
- 不允许为了恢复而把百万级 object ID 全部载入内存。
- 恢复后的最终 ref 发布必须复用 Sync Engine 的 pre-commit lease and intent validation。
- 如果 journal 显示对象已上传但远端 stat/read-back 不存在，必须退回 planned 或 failed 并重新上传，不得继续发布 ref。
- 长时间中断恢复后必须重新获取远端 lease/intent 新鲜度；旧 heartbeat 不构成 GC fencing。

重试策略：

```text
max_retries: 5
initial_backoff: 500ms
max_backoff: 30s
jitter: true
```

上传后校验：

- `stat` 校验大小。
- snapshot/ref 必须 read-back 或强校验。
- chunk 可抽样 read-back。
- 最终 CAS 前，snapshot 可达 manifest 和 chunk/pack 索引引用必须至少完成一次完整存在性校验。

### 6.14 GC / Verify / Repair

MVP：

- 不自动删除远端对象。
- 支持 `verify snapshot`。
- 支持 `verify object`。

P1：

```text
verify remote --sample
repair missing
gc --dry-run
gc --execute --grace-period 30d
```

GC 安全原则：

- GC 不能只凭 ref reachability 判断删除。
- GC 不能直接以远端 prefix/list 结果推断逻辑对象可达性，必须通过 `StorageLayout` 建立 logical refs 到 physical refs 的映射。
- destructive GC 必须确认远端没有 active write intent 或未过期 transaction lease。
- 无 remote lock/lease、无可靠 transaction marker、弱 list 的后端默认禁止 `gc --execute`。
- 对 single-writer 后端，`gc --execute` 只能在明确离线维护窗口执行。
- grace period 不能依赖单个客户端本地时钟，必须使用远端对象时间、远端 generation/ETag、transaction heartbeat 的远端观测时间和安全窗口。
- 如果后端不能提供可靠远端时间或等价版本语义，`gc --execute` 必须禁用。
- 对近期创建但未被 ref 指向的 snapshot，必须使用比普通 loose object 更长的 unpublished snapshot grace period，避免清理 CAS 失败后尚未 rebase 的本地游离快照。
- 删除任何 snapshot 前，必须确认不存在仍在 grace period 内的子孙 snapshot 依赖它作为 ancestor。

GC 流程：

```text
check backend capability
  -> acquire gc lease if supported
  -> read layout root and layout generation
  -> sample remote clock/version semantics
  -> list active write intents
  -> abort execute if active intent exists
  -> read all refs
  -> walk reachable snapshot/tree/file/chunk
  -> build reachable set
  -> resolve reachable ObjectId set through StorageLayout
  -> paged list physical refs through StorageLayout scope
  -> exclude objects newer than gc_safe_horizon
  -> exclude recent unreferenced snapshots and ancestors within unpublished grace period
  -> exclude objects covered by active or recent write intents
  -> find unreachable physical refs
  -> apply grace period
  -> dry-run report
  -> re-read refs, active intents, and layout root before delete
  -> execute with deletion journal
```

Write intent 与 GC fencing：

```text
transactions/active/<operation_id>.intent
```

GC 必须跳过或中止：

- 存在未过期 active intent。
- 存在 heartbeat 在 `gc_safe_horizon` 内的 intent。
- 后端 list 语义不足以可靠发现 active intent。
- 无法用远端 Last-Modified、generation、ETag 或等价能力确认 intent 新鲜度。
- 删除执行前复查发现 ref 或 intent 集合发生变化。
- 删除执行前复查发现 layout root generation 发生变化。

Layout rewrite 与 GC：

- pack compaction、历史强撤销重加密、ORAM reshuffle 都属于 layout rewrite。
- layout rewrite 发布新 root 前，旧 physical refs 必须保留到 rewrite grace period 之后。
- GC 必须同时理解 current layout root 和仍在 grace period 内的 previous layout roots。
- 删除 physical refs 前必须确认没有任何可读 layout generation 引用它们。

过期判定：

- `intent_expiry`、`gc_safe_horizon`、对象年龄和 grace period 都必须基于远端观测时间或远端版本推进。
- 本地 wall clock 不得单独决定对象是否可删。
- 客户端可以记录本地时间用于审计，但 GC 删除条件必须能从远端状态重新计算。
- 对 S3-compatible 后端，优先使用对象 `Last-Modified` 与条件请求语义；对 WebDAV/Alist 等弱后端，除非适配器通过准入测试证明时间和 list 可靠，否则只允许 dry-run。

默认安全窗口：

```text
gc_safe_horizon: max(30d, configured_max_push_duration * 3)
unpublished_snapshot_grace_period: >= 30d
intent_heartbeat_interval: 10m
intent_expiry: 72h 或用户配置
```

说明：

- 大型 push 可能持续数天，`gc_safe_horizon` 必须按产品目标配置。
- 如果用户希望清理更激进，必须显式传入参数并确认风险。
- dry-run 可以在弱后端执行，但 execute 必须受 capability 限制。

防腐：

- 随机抽样远端对象。
- 验证 AEAD。
- 验证 object ID。
- 本地有副本时重新上传修复。

## 7. 访问与呈现平面架构

### 7.1 职责

访问与呈现平面负责把核心能力暴露给用户和外部程序：

- CLI。
- Local Web UI。
- Local HTTP API。
- VFS。
- SDK / C-ABI。

### 7.2 模块树

```text
Access Plane
  1. CLI
  2. Local Web UI
  3. Local HTTP API
  4. VFS
  5. Rust SDK
  6. C-ABI
```

### 7.3 CLI

职责：

- 提供所有核心功能入口。
- 作为 MVP 主要用户界面。

命令草案：

```text
e2v init <repo>
e2v commit <path> -m "message"
e2v snapshots
e2v checkout <snapshot> <target>

e2v remote add <name> <url>
e2v push <remote> <branch>
e2v fetch <remote>
e2v clone <remote> <target>
e2v pull <remote> <branch>

e2v branch list
e2v branch create <name>
e2v branch checkout <name>

e2v serve --snapshot <id>
e2v search <query>

e2v verify snapshot <id>
e2v verify remote --sample 1%
e2v repair
e2v gc --dry-run
e2v gc --execute --grace-period 30d
```

技术：

- `clap`。

阶段：

- MVP。

### 7.4 Local Web UI

职责：

- 浏览 snapshot。
- 浏览目录。
- 下载文件。
- 预览浏览器支持的文件。
- 支持大文件 Range 请求。

设计：

- 默认绑定 `127.0.0.1`。
- 启动时生成随机 token。
- URL 不暴露真实 object ID。
- 不默认开放局域网访问。

技术：

- `axum`。
- streaming response。
- HTTP Range。

阶段：

- P0-C。

### 7.5 Local HTTP API

职责：

- 支撑 Web UI。
- 支撑外部本地集成。

规则：

- 默认 localhost。
- 必须 token 鉴权。
- 不暴露 object ID。
- 使用 snapshot-relative path 或 file handle。

阶段：

- P0-C 半稳定。
- P1 稳定化。

### 7.6 VFS

职责：

- 把远端密文仓库映射为本地可浏览文件系统。

阶段：

- P2-A：Linux FUSE 只读。
- P2-A2：Windows WinFSP 只读。
- P2-A3：macOS macFUSE 只读。
- P3：可写 VFS。

为什么后置：

- 依赖 logical range read 到 physical read plan 的映射。
- 依赖本地 cache。
- 依赖目录按需加载。
- 依赖错误恢复。
- 依赖文件句柄生命周期管理。

缓存策略：

| 缓存类型 | 默认 | 是否加密 | 说明 |
| --- | --- | --- | --- |
| remote object cache | 开启 | 是，保存密文 | 缓存 chunk/pack 密文 |
| manifest cache | 开启 | 本地 DB 加密 | 缓存 manifest |
| plaintext page cache | 内存 | 否 | 进程内 LRU |
| plaintext temp file | 关闭 | 否 | 用户显式开启时才允许明文临时文件 |

一致性模型：

- VFS 必须区分 snapshot-pinned mount 和 live branch mount。
- snapshot-pinned mount 的 inode/file handle 绑定不可变 snapshot，不受后台 fetch 影响。
- VFS 文件句柄必须绑定 snapshot id、file object id 和 layout generation。
- VFS 不得缓存或持久化远端 physical path；range read 必须通过 `StorageLayout` 生成 read plan。
- live branch mount 在 Sync Engine 更新 Local Index 或 ref view 后，必须通过 IPC、事件总线或共享订阅通知 VFS。
- VFS 收到 live branch 更新后，必须主动向内核发起目录项、inode、attribute 和 page cache invalidation。
- Linux FUSE 需要使用 `FUSE_NOTIFY_INVAL_INODE`、`FUSE_NOTIFY_INVAL_ENTRY` 或等价机制；Windows WinFSP 和 macFUSE 必须使用对应平台的 invalidation API。
- 如果平台或实现无法可靠 invalidation，live branch mount 必须关闭内核 page cache 或使用 direct I/O，以正确性优先。
- 已打开文件句柄必须绑定打开时的 snapshot/file object；不能在同一 handle 中混读旧版本和新版本 chunk。
- live branch 更新后，新打开的 handle 读取新版本，旧 handle 继续读取打开时版本，或返回明确的 stale handle 错误。

“零落盘”语义：

- 默认不把完整明文文件写入磁盘。
- 允许密文 cache 落盘。
- 允许明文短期内存 buffer。
- 明文临时文件必须由用户显式开启。

技术：

- Linux：`fuser`。
- Windows：WinFSP。
- macOS：macFUSE。

### 7.7 SDK / C-ABI

职责：

- 给第三方前端、桌面应用或其他语言调用核心能力。

Rust API 分层：

```text
e2v-core-api
  init_repo
  open_repo
  commit
  checkout
  list_snapshots
  verify_snapshot

e2v-sync-api
  add_remote
  push
  fetch
  clone

e2v-read-api
  open_snapshot
  read_dir
  open_file
  read_range
```

稳定性：

| API | 稳定等级 | 阶段 |
| --- | --- | --- |
| Rust internal crate API | 不稳定 | MVP |
| CLI | 半稳定 | MVP |
| Local HTTP API | 半稳定 | P0-C |
| Rust public API | 稳定 | P1 |
| C-ABI | 稳定 | P2 |

C-ABI 原则：

- 不暴露 Rust 泛型。
- 不暴露 Rust enum 内存布局。
- 使用 opaque handle。
- 所有错误通过错误码和错误字符串句柄返回。
- 提供显式 free 函数。

## 8. 运行时与可靠性平面架构

### 8.1 职责

运行时与可靠性平面负责让系统在真实环境下不乱：

- 并发调度。
- 内存背压。
- 本地锁。
- 崩溃恢复。
- 可观测性。
- 测试矩阵。
- 格式版本边界。

### 8.2 模块树

```text
Runtime & Reliability Plane
  1. Scheduler
  2. Backpressure
  3. Local Repo Lock
  4. Crash Recovery
  5. Observability
  6. Error Code
  7. Doctor Bundle
  8. Test Matrix
  9. Format Migration
```

### 8.3 Scheduler

技术：

- `tokio`：I/O、网络、任务编排。
- `rayon` 或 blocking pool：CPU 密集任务。

设计：

- CPU 密集任务不得阻塞 tokio runtime。
- 上传并发、hash 并发、encrypt 并发分别限流。
- I/O、CPU、上传三个执行域之间必须通过有界队列连接，禁止无界 producer。
- CPU 执行域不得执行网络或磁盘阻塞 I/O。
- Tokio runtime 不得同步等待 CPU 池中依赖 Tokio 消费结果的任务，CPU 池也不得同步等待 Tokio 任务释放容量。
- 如果使用 rayon，必须有专用池和清晰的返回通道，不得让全局 rayon 池参与可能反向等待 tokio 的流程。
- MVP 推荐使用 tokio blocking pool 或等价隔离执行上下文处理 hash/encrypt/chunking，避免跨 runtime 回调导致事件循环饿死。

阶段：

- MVP。

### 8.4 Backpressure

职责：

- 防止大文件或大目录提交时内存爆炸。

策略：

- 限制 in-flight chunk。
- 限制 pending upload queue。
- 限制明文 buffer。
- 限制 pack writer buffer。
- 背压必须从 uploader 向 encrypt/hash/chunker/reader 逐级传播。
- 当远端限流或上传停滞时，reader 和 chunker 必须停止产生新明文 buffer，而不是继续填满 CPU 队列。
- 有界队列容量必须按内存预算推导，不能只按任务数量配置。
- 背压等待必须可取消，避免 shutdown、错误恢复或重试时出现永久挂起。

默认目标：

```text
plain_memory_cache_limit: 512 MiB
max_inflight_chunks: configurable
max_concurrent_uploads: configurable
```

阶段：

- MVP 基础。
- P1 调优。

### 8.5 Local Repo Lock

职责：

- 防止同一仓库多个写操作同时进行。

锁文件：

```text
.e2v/lock
```

写操作需要锁：

- commit。
- push。
- pull。
- gc。
- repack。
- keyring update。

读操作可并发：

- list。
- serve。
- checkout immutable snapshot。
- verify snapshot。

过期锁：

- 必须确认进程不存在。
- 或由用户显式解除。

阶段：

- MVP。

### 8.6 Crash Recovery

职责：

- 进程崩溃后恢复到明确状态。

恢复策略：

| 上次阶段 | 策略 |
| --- | --- |
| scanning | 丢弃或重新扫描 |
| chunking | 丢弃或重新切块 |
| uploading objects | 检查已上传对象，继续上传 |
| writing snapshot | 检查 snapshot 是否存在，不存在则重写 |
| updating ref | 读取 ref，判断 CAS 是否成功 |
| publishing layout root | 读取 layout root generation，判断发布是否成功 |
| gc dry-run | 丢弃 |
| gc execute | 读取删除日志，继续或报告人工确认 |
| repack | 未发布 pack index 的 partial pack 可清理 |
| layout rewrite | 读取 rewrite journal，继续发布或保留旧 layout generation |

阶段：

- P0-B。

### 8.7 Observability

结构化日志字段：

```text
timestamp
level
operation_id
repo_id_prefix
backend
action
object_type
object_id_prefix
duration_ms
error_code
```

不得记录：

- 明文路径，除非用户开启 verbose local diagnostics。
- 明文文件名。
- 密钥材料。
- 完整 object ID，默认只显示前缀。

错误码：

```text
E_CRYPTO_AUTH_FAILED
E_OBJECT_NOT_FOUND
E_OBJECT_CORRUPT
E_REF_CONFLICT
E_BACKEND_UNSUPPORTED_CAPABILITY
E_BACKEND_RATE_LIMITED
E_LOCAL_LOCKED
E_JOURNAL_RECOVERY_REQUIRED
E_KEYRING_UNLOCK_FAILED
E_FORMAT_UNSUPPORTED
E_BACKEND_SINGLE_WRITER_ONLY
E_REMOTE_WRITE_INTENT_ACTIVE
E_GC_FENCING_UNAVAILABLE
E_PACK_INDEX_CONFLICT
E_LAYOUT_ROOT_CONFLICT
E_LAYOUT_REF_UNRESOLVED
```

诊断命令：

```text
e2v doctor
e2v doctor --bundle
```

诊断包不包含明文路径和密钥。

### 8.8 Test Matrix

正确性测试：

- 同一文件重复 commit，不重复存储 chunk。
- 大文件中间插入小片段，只新增局部 chunk。
- checkout 后内容与源目录一致。
- branch 创建不复制对象。
- macOS/APFS、Windows、Linux 下 NFC/NFD、大小写折叠、组合音标路径的 scan -> checkout -> scan 必须幂等。
- commit 期间源文件 size、mtime 或 change id 变化时，必须丢弃该文件本次临时对象并重试或报错。
- 对长时间读取的大文件，平台快照视图和读前/读后校验路径都必须覆盖测试。

安全测试：

- 篡改 chunk，checkout 必须失败。
- 篡改 tree，遍历必须失败。
- 替换 object type，AEAD AD 必须检测。
- 错误密码无法解锁 keyring。
- 裸公开文件无法推导 object ID。
- 被撤销设备拿到旧 epoch key 后，不得解密撤销后新 epoch 对象。
- 对象缺少当前 schema 的必需字段时必须拒绝解码。
- 启用 padding policy 后，篡改 padding 或 padding length 必须导致 AEAD 或解码失败。
- checkout、HTTP Range、VFS 不得暴露未完成 AEAD 验证的明文字节。
- AEAD 验证失败时，checkout 临时输出必须被清理，最终路径不得出现脏文件。

同步测试：

- push 中断后恢复。
- ref CAS 冲突。
- 两客户端同时 push。
- 无 CAS 后端两客户端同时 push 必须被远端 writer lease 拒绝；无可靠 lease 时禁止后台自动 push。
- 远端对象缺失。
- 弱 list 后端下禁用危险 GC。
- active write intent 存在时，`gc --execute` 必须中止。
- write intent heartbeat 过期前，GC 不得删除相关时间窗口内对象。
- write intent 过期后恢复的 push，在最终 ref CAS 前必须重新 verify 可达对象并续期 lease/intent。
- CAS 失败后基于 unpublished snapshot 再次 push 时，必须验证远端 parent chain 完整或补传缺失 ancestor。
- GC 删除前 ref 或 intent 集合变化时，必须中止或重新计算。
- GC 不得删除仍在 unpublished snapshot grace period 内的无 ref snapshot 及其 ancestor。
- 客户端本地时钟大幅偏移时，GC 和无人值守 push 不得依赖本地时间完成危险操作。
- Operation Journal 在百万对象上传时不得整文件重写。
- layout root 发布与 ref CAS 交错失败时，恢复后不得出现 ref 指向 layout 不可解析对象。
- pack compaction、历史强撤销、ORAM reshuffle 产生的新 layout generation 必须可恢复发布或安全回滚。

VFS 测试：

- snapshot-pinned mount 后台 fetch 不得改变已打开文件句柄内容。
- live branch mount 后台 fetch 后必须 invalidation 目录项、inode、attribute 和 page cache。
- 无可靠 invalidation 的平台必须使用 direct I/O 或关闭内核 page cache。
- 已打开文件句柄不得混读旧版本和新版本 chunk。

崩溃测试：

- uploading chunk。
- writing snapshot。
- updating ref。
- publishing layout root。
- writing pack。
- publishing pack index。
- updating index root。
- layout rewrite。
- gc execute。

调度测试：

- 远端上传限流时，reader/chunker/hash/encrypt 必须因背压停止继续扩大内存占用。
- CPU 执行域和 tokio I/O 执行域互相等待时不得死锁，错误和取消必须能传播。
- 有界队列容量耗尽时，系统必须进入可观测等待状态，而不是 CPU 归零永久挂起。

性能测试数据集：

| 数据集 | 规模 | 用途 |
| --- | --- | --- |
| tiny files | 100k files, 1-16 KiB | 小文件压力 |
| source tree | 1M LOC | 工程目录 |
| large binary | 10 GiB file | 大文件增量 |
| media set | 10k images/videos | 混合数据 |
| history | 500 snapshots | 索引和历史查询 |

### 8.9 Format Boundary

每个加密对象 header 必须包含：

```text
format_version
object_type
crypto_suite
key_epoch
padding_policy
```

按对象类型进入 canonical plaintext schema 的字段：

| 字段 | 必需对象 | 说明 |
| --- | --- | --- |
| `chunker_id` | `file` | 标识该文件版本使用的 chunker |
| `chunker_config_id` | `file` | 标识 chunker 参数，参数变化必须改变该值 |
| `path_policy` | `snapshot` 或 repo config | 固定 tree/path 解释规则 |
| `layout_generation` | ref view、layout root、operation journal 或 read handle | 绑定读取/发布时使用的 layout generation；不得进入 immutable Snapshot DAG |
| `schema_version` | 所有 manifest plaintext | 约束 canonical decoding，不允许隐式忽略未知字段 |

仓库配置包含：

```text
repo_format_version
min_client_version
active_epoch
```

规则：

- 当前阶段只定义单一 repo format 的边界和必需字段。
- 所有对象必须带 `format_version`，用于拒绝处理不属于当前 schema 的对象。
- schema 变更必须显式升级 `repo_format_version`，不能靠“忽略未知字段”继续写入。
- 对象编码必须 canonical，确保相同逻辑对象在不同客户端生成相同 object ID。
- 格式演进必须通过新的 `repo_format_version` 设计文档进入计划。

## 9. Rust 工程结构

建议 workspace：

```text
crates/
  e2v-core/
    facade.rs
    read_service.rs
    working_tree.rs
    object/
    crypto/
    chunk/
    manifest/
    snapshot/
    branch/

  e2v-store/
    layout.rs
    logical_object_store.rs
    blob_store.rs
    ref_store.rs
    layout_root_store.rs
    capability.rs
    local_backend.rs
    opendal_backend.rs

  e2v-sync/
    push.rs
    fetch.rs
    clone.rs
    cas.rs
    journal.rs
    transaction.rs
    publisher.rs

  e2v-index/
    sqlite.rs
    rebuild.rs
    query.rs

  e2v-pack/
    pack_writer.rs
    pack_index.rs
    range_reader.rs
    rewrite.rs

  e2v-cli/
    main.rs

  e2v-web/
    server.rs
    browse.rs
    range.rs

  e2v-vfs/
    fuse_readonly.rs

  e2v-api/
    rust_api.rs
    c_abi.rs
```

依赖规则：

- `e2v-core` 不依赖具体存储后端。
- `e2v-core` 不依赖 CLI/Web/VFS。
- `e2v-core::facade` 是 CLI/Web/SDK 的主要入口；CLI 不直接调用底层 store 或 sync 模块。
- `e2v-core::working_tree` 是唯一允许直接处理工作目录路径策略和 checkout 写入副作用的核心模块。
- `e2v-core::read_service` 是 Web/HTTP/VFS/SDK 的统一只读入口。
- `e2v-store` 不理解明文文件语义。
- `e2v-store::logical_object_store` 封装 encode/hash/encrypt/decrypt/layout resolve，不把 physical refs 暴露给 ManifestStore。
- `e2v-store` 必须通过 `StorageLayout` 暴露逻辑对象到物理对象的映射，不允许上层依赖远端路径规则。
- `e2v-sync` 负责编排上传、下载和 ref/layout 发布。
- `e2v-sync::publisher` 是唯一允许组合 writer lease、write intent、layout root CAS 和 ref CAS 的模块。
- `e2v-index` 可删除重建。
- `e2v-vfs` 只能依赖稳定 read/browse API。
- `e2v-api` 不绕过编排层。

## 10. 技术选型总表

| 子系统 | 技术 | 阶段 | 说明 |
| --- | --- | --- | --- |
| 语言 | Rust | MVP | 内存安全、系统工具、跨平台 |
| async runtime | tokio | MVP | I/O 和任务编排 |
| CPU 并行 | rayon / blocking pool | MVP/P1 | hash/encrypt/chunking |
| 后端抽象 | opendal | MVP | S3/WebDAV/local 等适配 |
| 存储布局 | StorageLayout | MVP/P1/P3 | direct / pack / rewrite / ORAM layout |
| 逻辑对象读写 | LogicalObjectStore | P0-A | 封装 encode/hash/encrypt/decrypt/layout resolve |
| 只读访问服务 | ReadService | P0-A/P0-C/P2 | 统一 Web/HTTP/VFS/SDK 的 snapshot/dir/file/range 读取 |
| 发布协议 | TransactionPublisher | P0-B/P1/P3 | 统一 write intent、layout root CAS、ref CAS 和恢复 |
| 后端写者模式 | CAS / single-writer + writer lease | P0-B | 无 CAS 后端禁止多写者，single-writer 必须有远端 lease |
| CDC | fastcdc | MVP | 通用内容定义切块 |
| Chunking Policy | profile + dedup stats | P1 | 处理高熵和大二进制退化 |
| Hash | blake3 keyed mode | MVP | 去重且不暴露裸明文 hash |
| KDF | argon2id | MVP | 用户密码解锁 |
| AEAD | XChaCha20-Poly1305 / AES-GCM | MVP | 优先降低 nonce 误用风险 |
| 本地索引 | SQLite / FTS5 | P1 | 搜索和 cache |
| 大规模 KV cache | RocksDB | P2 | 极大规模索引 |
| CLI | clap | MVP | 命令行 |
| Web | axum | P0-C | 本地预览 |
| VFS | fuser / WinFSP / macFUSE | P2 | 只读优先 |
| API | Rust API / C-ABI | P1/P2 | 外部集成 |
| Path Policy | portable-strict + escaped checkout | MVP/P1 | 跨平台路径兼容 |
| Operation Journal | SQLite / append-only WAL | P0-B | 海量对象断点续传状态 |
| Pack Index | immutable segment + compaction | P1 | 避免全局 index 覆盖和千次请求 |
| 历史强撤销 | layout rewrite + re-encryption | P3 | 通过新 layout generation 发布重加密对象 |
| 访问模式隐藏 | ORAM-style StorageLayout | P3 | 隔离访问调度与核心 Object Model |

## 11. 原计划覆盖确认

| 原计划能力 | v4 状态 | 所属子架构 |
| --- | --- | --- |
| CDC 增量去重 | 已保留并加强 | 数据平面 |
| S3/WebDAV 远端同步 | 已保留并阶段化 | 存储与同步平面 |
| Chunk 级断点续传 | 已保留并具体化 | 存储与同步平面 |
| 弱后端并发写限制 | 已补强 | 存储与同步平面 |
| 无中心 GC 安全边界 | 已补强 | 存储与同步平面 |
| Operation Journal 可扩展性 | 已补强 | 存储与同步平面 |
| Pack Index 分层 | 已补强 | 数据平面 |
| E2EE 内容和元数据加密 | 已保留并加强 | 密码学平面 |
| Keyring 安全更新 | 已补强 | 密码学平面 |
| Web UI + VFS 双轨预览 | 已保留并排序 | 访问与呈现平面 |
| 零成本分支 | 已保留 | 元数据与版本平面 |
| 本地毫秒级检索 | 已保留并分层 | 元数据与版本平面 |
| 单级巨型目录处理 | 已补强 | 元数据与版本平面 |
| 稀疏检出/浅克隆 | 已保留 | 元数据与版本平面 |
| Bit-rot 防腐与自愈 | 已保留 | 存储与同步平面 |
| 第三方 API / C-ABI | 已保留并补契约 | 访问与呈现平面 |
| Chunker 策略扩展 | 已保留并补契约 | 数据平面 |
| 高熵数据 chunking 退化 | 已补强 | 数据平面 |
| 跨平台路径兼容 | 已补强 | 数据平面 / 元数据与版本平面 |
| ManifestStore / N+1 / RocksDB | 已保留并补契约 | 元数据与版本平面 |
| tokio + rayon | 已保留 | 运行时与可靠性平面 |
| opendal | 已保留 | 存储与同步平面 |
| fastcdc + blake3 | 已保留并修正 | 数据平面 |
| aes-gcm + argon2 | 已保留并扩展 | 密码学平面 |
| rusqlite + r2d2 | 已保留但降权为 cache | 元数据与版本平面 |
| fuser / WinFSP | 已保留并后置 | 访问与呈现平面 |

## 12. 架构风险树

```text
高风险
  - keyring 损坏导致仓库永久不可解密
    -> generation + atomic publish + fsync + 保留历史 keyring
  - 加密去重泄露已知明文
    -> keyed hash，不做跨仓库去重；同仓库频率泄露列入明确隐私边界
  - AEAD nonce 复用
    -> 派生 nonce 或使用大 nonce AEAD
  - ref 并发覆盖
    -> CAS；无 CAS 后端必须使用远端 writer lease，否则禁用常规 push
  - 无中心 GC 幽灵删除正在上传的对象
    -> write intent / transaction lease / gc fencing / pre-commit verify / 弱后端禁用 execute
  - 客户端时钟偏移导致 GC 误删
    -> GC 和 intent expiry 基于远端时间或版本语义，本地时钟不能作为删除事实源
  - CAS 失败后的游离 snapshot 被 GC 后造成 parent 断裂
    -> unpublished/needs-rebase 状态、pre-push ancestor closure 校验、unpublished snapshot grace period
  - schema 边界不清导致字段被静默忽略
    -> 当前阶段采用显式 schema，未知格式拒绝处理，禁止隐式忽略字段
  - 活跃大文件 torn read 形成不存在的快照内容
    -> 平台快照优先；读前/读后 metadata 校验；变化则丢弃临时对象并重试或报错
  - 未认证明文提前暴露到本地或 VFS/HTTP
    -> chunk/内部段级认证；临时输出；认证失败清理并拒绝发布
  - P0 CLI 直连底层模块导致 Web/VFS/SDK 反向重构
    -> P0-A 前置 Repository Facade、WorkingTree、LogicalObjectStore、ReadService；CLI 只调用 facade
  - layout root 与 ref 发布协议散落导致不可恢复状态
    -> P0-B 引入 TransactionPublisher，统一 writer lease、write intent、layout root CAS、ref CAS 和 recovery

中高风险
  - JSON 大 Journal 导致内存和 I/O 爆炸
    -> SQLite object state table 或 append-only WAL
  - tokio 与 CPU 池背压反噬导致死锁
    -> 有界队列、执行域隔离、背压向 reader 传播、等待可取消
  - Pack Index 全局覆盖或分散读取瓶颈
    -> immutable index segment / local cache / compaction
  - P3 历史强撤销或 ORAM 推翻 P0/P1 存储设计
    -> P0 引入 StorageLayout；核心只依赖 ObjectId；GC/VFS/pack 只通过 layout 解析 physical refs
  - 单级巨型目录导致 manifest 内存爆炸
    -> Tree Sharding / max manifest size / streaming tree walk；若 MVP 覆盖 100k+ 小文件则 P0 前置
  - 小对象数量爆炸
    -> packfile
  - 高熵或压缩数据导致 CDC 去重退化
    -> Chunking Policy Engine / dedup stats / format-aware roadmap
  - VFS 复杂度失控
    -> P2 只读，依赖 logical range read、StorageLayout read plan 和 cache
  - VFS live branch page cache 读到旧视图或混合版本
    -> snapshot-pinned handle 语义、live mount invalidation、必要时 direct I/O
  - 元数据隐私与稀疏检出冲突
    -> 逐层解密；P1 实现路径 token，仓库启用时记录隐私降级
  - 加密对象大小和流量模式泄露数据轮廓
    -> P1 padding policy 降低大小指纹；P3 ORAM/访问模式隐藏降低访问模式泄露
  - WebDAV 能力不稳定
    -> capability detection，弱能力降级
  - 设备撤销后继续读取未来数据
    -> active_epoch + key_epoch + keyring envelope 移除；旧 epoch 只读

中风险
  - 跨平台路径无法 checkout
    -> portable-strict 默认策略 / path validation dry-run / 平台 read-back 映射 / escaped checkout roadmap
  - SQLite 被误当事实源
    -> 本地 cache，可删除重建
  - 本地索引泄露明文路径
    -> 明确本地威胁边界，本地 manifest cache DB 加密
  - 多设备撤销语义复杂
    -> P2 撤销未来访问；P3 历史强撤销通过全仓库重加密移除旧 epoch 访问能力
```

## 13. 架构决策清单

### 13.1 MVP 前

| 问题 | 已定决策 |
| --- | --- |
| canonical encoding | 明确版本约束的二进制编码 |
| AEAD | XChaCha20-Poly1305 优先 |
| object ID | 必须 keyed |
| ref | 默认加密 |
| keyring 更新 | generation + atomic rename + fsync + 保留历史版本 |
| path policy | portable-strict |
| 路径规范化 | checkout 后 read-back 校验，macOS/APFS 纳入测试 |
| manifest 上限 | 设置 max manifest size 和 max tree entries |
| schema 边界 | 显式 schema；未知格式拒绝处理 |
| padding policy | MVP 支持 `none` 和 manifest 随机 padding；P1 支持固定桶 padding |
| 源文件一致性 | 大文件优先平台快照；否则读前/读后 metadata 校验 |
| 解密认证边界 | 未完成 AEAD 验证的明文不得暴露到最终输出或响应 |
| StorageLayout | MVP 引入 direct layout，禁止核心依赖远端物理路径 |
| Repository Facade | P0-A 前置，CLI/Web/SDK 不直接调用底层模块 |
| WorkingTree | P0-A 前置，统一 scan、路径策略、源文件一致性和 checkout 写入 |
| LogicalObjectStore | P0-A 前置，封装逻辑对象 encode/hash/encrypt/decrypt/layout resolve |
| ReadService | P0-A 前置内部 API，P0-C Web/HTTP 复用，P2 VFS 复用 |
| 本地索引 | P1 实现 SQLite + FTS5 |

### 13.2 P0-B 前

| 问题 | 已定决策 |
| --- | --- |
| 首个远端后端 | S3-compatible |
| WebDAV | P1-B |
| ref CAS | 条件写优先；无 CAS 后端只能 single-writer + 远端 writer lease |
| writer mode | multi-writer 需要 CAS；WebDAV 默认 single-writer + 远端 writer lease |
| upload journal | SQLite 或 append-only WAL，不记录明文，uploaded 不等于最终存活证明 |
| remote write intent | P0-B 设计，供 GC fencing 使用；最终 CAS 前必须校验新鲜度 |
| GC 时间源 | 远端 Last-Modified/generation/ETag 或等价语义，本地时钟不能作为事实源 |
| read-back 校验 | snapshot/ref 必须；最终 CAS 前校验可达对象存在性 |
| ancestor closure | push 前验证 parent chain 远端完整，CAS 失败 snapshot 标记 needs-rebase |
| unpublished snapshot GC | 无 ref snapshot 使用 >=30d grace period，并保留其 ancestor |
| TransactionPublisher | P0-B 前置，统一 writer lease、write intent、layout root CAS、ref CAS 和恢复 |
| layout root | direct layout 也必须有 root 和 generation，P1/P3 复用发布协议 |

### 13.3 P1 前

| 问题 | 已定决策 |
| --- | --- |
| packfile | 加密整体 pack |
| pack index | 不可变 segment + 本地 cache + compaction；索引指向 physical refs 而不是远端路径 |
| Tree Sharding | P1 必须实现；若 MVP 覆盖十万级小文件则 P0-A 前置 |
| Chunking Policy | 增加 large-binary profile 和 dedup stats |
| SQLite 路径索引 | 允许本地保存路径信息；manifest cache DB 加密；明确隐私影响 |
| 路径 token 索引 | P1 实现；仓库启用时记录隐私降级 |
| GC | 默认 dry-run；execute 需要 transaction/lease/fencing 能力 |
| 背压模型 | 有界队列贯穿 reader/chunker/hash/encrypt/uploader，执行域互不反向阻塞 |

### 13.4 P2 前

| 问题 | 已定决策 |
| --- | --- |
| VFS | 第一版只读 |
| VFS 一致性 | snapshot-pinned 默认；live branch 必须支持 cache invalidation 或 direct I/O |
| 明文缓存 | 默认不落盘 |
| 多设备撤销 | P2 key epoch 轮转阻止未来访问；P3 历史强撤销重加密旧对象 |
| C-ABI | Rust API 稳定后发布 |

### 13.5 P3 已定决策

| 问题 | 已定决策 |
| --- | --- |
| encrypted remote search | 排除，不进入路线图 |
| 历史数据强撤销 | P3 实现；通过 layout rewrite、全仓库重加密、旧 epoch 退休和对象重写 journal 发布 |
| ORAM / 访问模式隐藏 | P3 实现；通过 ORAM-style StorageLayout、padded access schedule、traffic shaping 和成本策略实现 |
