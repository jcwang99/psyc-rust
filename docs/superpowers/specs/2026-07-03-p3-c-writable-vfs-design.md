# P3-C 可写 VFS 设计

## 背景

P2-A 引入了平台无关的只读 VFS 核心，并通过 WinFSP 适配把 `snapshot` / `live branch`
两种挂载视图暴露给宿主系统。P3-B 又补齐了访问模式隐藏与 direct I/O 退化策略，但当前
挂载仍然只支持只读语义，无法满足“在分支挂载点内直接编辑文件并原子提交回分支”的目标。

P3-C 的目标是在不破坏既有只读边界的前提下，为 `live branch` 挂载补齐可写能力，并满足：

1. `writable mount`
2. `conflict handling`
3. `atomic writeback`
4. `cache coherency`

## 目标

### 功能目标

1. 只有 `live branch` 挂载支持写入，`snapshot` 挂载继续保持只读。
2. 写入先进入挂载内的 overlay，而不是直接修改宿主仓库工作树。
3. 挂载层能够把 overlay 作为一次原子 writeback 提交到目标分支。
4. 当分支头或目标路径与 overlay 冲突时，writeback 必须失败并保留本地 overlay。
5. writeback 成功后，新的读请求必须立即观察到新视图，目录项与 inode 缓存不能继续暴露旧内容。

### 非目标

1. 不兼容旧版写入路径，统一以新的 overlay + atomic writeback 语义为准。
2. 不在 P3-C 内实现 Linux/macOS 的内核级可写挂载，仅定义平台适配边界和测试契约。
3. 不支持内存映射写、字节范围锁、内核 writeback cache 等需要强平台协同的语义。
4. 不让 VFS 核心直接访问 `ManifestStore`、`LogicalObjectStore`、`StorageLayout` 或物理仓库路径。

## 用户体验

### 可写分支挂载

用户通过 `e2v-cli mount branch ...` 启动 `live branch` 挂载时，可以在挂载点内：

- 创建、覆盖、截断、删除普通文件
- 创建和删除目录
- 重命名文件或目录
- 读取自己尚未 writeback 的未提交修改

这些修改不会立刻落到真实仓库工作树，而只存在于挂载 overlay 中。

### 原子写回

平台层在显式 writeback 或挂载释放阶段触发 writeback 时，系统会：

1. 读取当前分支最新 head
2. 检查 overlay 基线是否仍可快进应用
3. 若无冲突，则一次性生成新的树与提交并推进分支
4. 若有冲突，则返回冲突详情并保留 overlay，等待用户重试或放弃

## 架构

### 分层边界

`crates/e2v-vfs`

- 承载平台无关的 VFS 核心
- 维护 `snapshot` / `live branch` 两种命名空间视图
- 负责 inode/path 解析、文件句柄绑定、只读/可写语义判断
- 负责 overlay 读写、冲突检查、cache invalidation 与 direct I/O 退化策略判断
- 只通过 `e2v-core::ReadService` 读取基线内容
- 通过新的写回抽象向 `e2v-core` 请求原子提交，不直接操作物理工作树

`crates/e2v-core`

- 提供“以虚拟工作树为输入”的写回边界
- 负责把 overlay 描述转换成 tree/commit 更新
- 负责 branch head 校验、冲突报告与原子 ref 更新

平台层

- Windows 先接 WinFSP，把宿主写操作映射到 VFS overlay 语义
- Linux/macOS 先保留 adapter trait / capability summary / 测试边界
- 不在平台层复制 overlay、冲突处理或缓存一致性逻辑

### 核心对象

#### `WritableVfs` / `LiveBranchVfs`

在现有只读核心基础上补充可写 live branch 模式，核心状态包含：

- `base_snapshot`: 当前挂载绑定的分支基线快照
- `overlay`: 记录未提交改动的内存模型
- `inode_table`: 路径与 inode 的稳定映射
- `handle_table`: 已打开文件句柄及其绑定的读写视图
- `cache_epoch`: 用于驱动目录项和属性缓存失效

#### `OverlayState`

overlay 是 live branch 写入的唯一真实来源，支持：

- `UpsertFile { path, bytes, executable }`
- `DeleteFile { path }`
- `CreateDirectory { path }`
- `DeleteDirectory { path }`
- `Rename { from, to }`

overlay 对外提供：

- 路径归一化与父目录约束检查
- 针对目录列举和属性查询的遮蔽结果
- 对文件句柄的 staged 内容视图
- dirty path 集合与结构性冲突检测输入

#### `BranchWritebackService`

新增 `e2v-core` 写回边界，职责：

1. 接收 `base_branch_head + base_snapshot + overlay mutations`
2. 校验目标分支当前 head 是否仍与基线兼容
3. 在内存中生成新的 tree
4. 创建提交并以 compare-and-swap 方式更新分支 ref
5. 返回新的 branch head、快照根和冲突详情

VFS 核心不感知底层 tree/commit/ref 的构造细节。

## 数据流

### 读路径

1. 请求先命中 overlay
2. 若 overlay 对路径有 upsert/delete/rename 遮蔽，则返回 overlay 视图
3. 若 overlay 未覆盖，则回退到 `ReadService`
4. 目录列举会把 base snapshot 与 overlay 结果做合并/过滤

### 写路径

1. 平台层把 `create/write/truncate/rename/unlink/mkdir/rmdir` 转成 VFS 核心调用
2. VFS 核心先做语义校验与路径约束
3. 修改写入 overlay，并更新受影响 inode/path 映射
4. 相关目录项缓存、属性缓存、句柄可见性立即切换到 overlay 视图

### writeback 路径

1. 从 VFS 读取当前 overlay 快照与基线 branch head
2. 调用 `BranchWritebackService`
3. 如果 branch head 已变化，则做冲突分析
4. 冲突为空则原子推进分支并返回新快照
5. VFS 用新快照替换基线，清空 overlay，递增 `cache_epoch`
6. 后续新打开的句柄都读到新基线内容

## 冲突模型

### 检测策略

writeback 前至少校验两类冲突：

1. `head changed`
   - 挂载基线 head 与当前分支 head 不一致
2. `path conflict`
   - 当前分支新变化与 overlay dirty path 相交
   - 或 overlay 的结构修改与远端结构变化冲突

### 冲突结果

失败时返回结构化冲突列表，至少包含：

- 冲突路径
- 冲突类型
- 基线路径状态
- 当前分支路径状态
- overlay 意图

失败不会清空 overlay，用户可以继续查看、导出或重试。

## 缓存一致性

### 句柄视图

已打开句柄继续绑定其打开时的 overlay/base 组合视图，但：

- 同一挂载中新打开句柄必须读取最新 `cache_epoch`
- 已写句柄在 `flush/fsync/release` 后的再次读取必须能看到自身 staged 数据

### 元数据与目录项

以下情况会触发缓存失效：

1. overlay 路径被修改
2. rename 影响源目录或目标目录
3. writeback 成功切换基线快照

失效后不得继续返回旧的文件长度、目录成员或不存在判断。

## direct I/O 与能力边界

由于当前无可靠内核失效回调，live branch 可写挂载默认维持 direct I/O / stream-only
路径，不启用依赖宿主页缓存一致性的 writeback cache 语义。以下语义继续显式拒绝：

- `MemoryMappedWrites`
- `ByteRangeLocks`
- `WritebackCaching`

`WritableHandles` 仅对 live branch 挂载开放。

## 测试策略

### 核心测试

1. snapshot 挂载拒绝所有写语义
2. live branch 挂载允许 create/write/truncate/delete/rename/mkdir/rmdir
3. overlay 读写对目录列举和属性查询立即可见
4. writeback 成功后 overlay 清空且基线快照推进
5. branch head 变化或 dirty path 冲突时 writeback 失败且 overlay 保留
6. writeback 成功后缓存刷新，新打开句柄读取新基线

### 平台边界测试

1. Windows 适配把宿主写操作正确路由到核心 overlay
2. Linux/macOS capability summary 与 trait 边界正确暴露“待实现的可写平台适配”

## 风险与缓解

### tree 构造与路径冲突复杂度

通过把 tree/commit/ref 更新集中在 `e2v-core` 写回边界内，避免平台层和 VFS 层重复实现
Git 树操作。

### cache coherency 容易退化

统一由 `cache_epoch` 与路径级失效表控制可见性，避免目录项缓存与句柄状态分散维护。

### Windows 首发平台风险

先保证平台层足够薄，仅做 WinFSP 事件映射。复杂逻辑全部留在跨平台核心测试中验证。
