# P3-C 可写 VFS 实施计划

## 目标

完成 `plan.md` 中 P3-C 的四项目标：

1. `writable mount`
2. `conflict handling`
3. `atomic writeback`
4. `cache coherency`

并遵循以下约束：

- 只让 `live branch` 挂载可写，`snapshot` 挂载保持只读
- 所有写入先进入 overlay，不直接修改宿主仓库工作树
- 通过 TDD 推进，每个能力都先补失败测试
- 平台层保持薄适配，核心逻辑集中在 `e2v-vfs` 与 `e2v-core`

## 阶段 1：核心写回边界

### 先写测试

1. 为 `e2v-core` 补充“从虚拟工作树/overlay 原子写回分支”的失败测试
2. 覆盖成功推进分支、head 变化冲突、dirty path 冲突三类场景

### 再做实现

1. 新增 `BranchWritebackService` 或等价核心抽象
2. 以 `base head + overlay mutations` 构造新 tree/commit
3. 使用 compare-and-swap 更新分支 ref
4. 返回结构化冲突信息与新 head / snapshot 元数据

## 阶段 2：VFS overlay 可写模型

### 先写测试

1. `snapshot` 挂载继续拒绝 `WritableHandles`
2. `live branch` 挂载允许 create/write/truncate/delete/rename/mkdir/rmdir
3. overlay 修改后，读取、属性查询、目录列举立即可见

### 再做实现

1. 在 `e2v-vfs` 中引入 live branch 可写状态机
2. 增加 overlay 数据结构、dirty path 跟踪与路径约束校验
3. 把文件句柄绑定到 staged 视图，确保自读一致

## 阶段 3：writeback 与缓存一致性

### 先写测试

1. writeback 成功后 overlay 清空、基线快照推进
2. writeback 失败时 overlay 保留并暴露冲突详情
3. writeback 成功后，新句柄和目录查询都看到新基线内容
4. rename / delete / truncate 会触发必要的缓存失效

### 再做实现

1. 将 VFS writeback 流程接到 `e2v-core` 写回边界
2. 引入 `cache_epoch` 与路径级失效
3. 统一更新 inode/path 解析缓存与目录项视图

## 阶段 4：平台适配

### 先写测试

1. Windows 边界测试覆盖写打开、写入、flush/release、rename/unlink/mkdir/rmdir 路由
2. Linux/macOS capability summary 反映“可写核心已就绪，平台适配待实现”

### 再做实现

1. 让 WinFSP 上下文接入可写 live branch VFS
2. 维持 snapshot 挂载只读与 direct I/O 退化策略
3. 为其他平台补齐 trait 和错误边界

## 阶段 5：自检与优化

1. 跑目标 crate 测试与关键集成测试
2. 复查是否仍有重复路径解析、冗余缓存失效或可合并的状态结构
3. 执行 `cargo fmt --all`
4. 汇总剩余限制，仅保留明确的非目标项
