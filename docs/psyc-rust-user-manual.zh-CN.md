# psyc-rust 用户手册

本手册面向 `psyc-rust` 的实际使用者，重点介绍如何在 Windows PowerShell 环境下初始化仓库、提交快照、配置远端、执行同步、进行共享协作，以及使用高级运维能力维护仓库健康状态。

本文档覆盖当前 CLI 中已经实现并通过测试验证的稳定功能，以及一组需要谨慎使用的高级运维命令：`gc`、`historical-rewrite`、`oram`、`serve`。文档默认假设你直接使用工作区内的 CLI 二进制，也就是通过 `cargo run -p e2v-cli -- ...` 执行命令。

## 1. 项目是什么

`psyc-rust` 是一个本地优先、支持快照、分支、远端同步和共享协作的内容仓库系统。它把工作目录中的文件整理成加密后的仓库状态，并通过快照记录版本历史。

从用户角度看，你可以把它理解成一套带有以下特性的工具：

- 用 `init` 创建一个受保护的仓库。
- 用 `commit` 把当前工作目录保存成快照。
- 用 `snapshots`、`checkout`、`branch` 查看和切换历史状态。
- 用 `remote`、`push`、`fetch`、`pull`、`clone` 做远端同步。
- 用 `share` 做成员和设备级共享。
- 用 `verify`、`repair`、`doctor`、`gc` 等命令检查和修复仓库。
- 用 `historical-rewrite` 和 `oram` 处理更高级的安全与访问模式需求。
- 用 `serve` 在本地启动 Web 只读浏览界面。

## 2. 手册适用范围

本手册默认覆盖以下范围：

- 使用环境：Windows PowerShell
- 接口形态：CLI
- 典型使用者：仓库拥有者、普通同步使用者、共享协作者、运维人员
- 功能深度：稳定功能 + 高级运维

本手册不深入讲解以下内容：

- Rust crate 级架构细节
- C ABI 或 SDK 的二次开发接口
- VFS `mount` 命令的完整平台级使用流程

## 3. 目录结构与基本概念

### 3.1 仓库目录

当你初始化仓库后，仓库根目录下会出现一个隐藏控制目录：

```text
<repo>\
  .e2v\
```

`.e2v` 用来存储控制平面、对象、密钥相关状态、默认远端配置和其他内部元数据。日常使用时，你不应该手工编辑这里的文件；如果这里出现损坏，优先使用 `verify`、`repair` 或 `doctor`，而不是直接改 JSON。

### 3.2 快照

每次 `commit` 都会把当前工作目录状态记录为一个不可变快照。快照有：

- 唯一的 `snapshot_id`
- 提交消息
- 对应的文件树内容

快照是后续 `checkout`、同步、共享验证和历史操作的基础单位。

### 3.3 分支

仓库至少有一个当前分支。默认初始化时分支名是 `main`。分支内部还对应一个内部 `branch token`，很多同步命令实际上依赖这个 token 识别当前分支状态。

### 3.4 默认远端

使用 `remote add` 后，仓库会保存一个默认远端注册信息。之后的 `push`、`fetch`、`pull`、`verify remote`、`repair`、`gc`、`historical-rewrite`、`oram`、`doctor` 等命令都会默认走这个远端。

### 3.5 本地优先

本项目的常见工作模式是：

1. 在本地工作目录改文件
2. 提交到本地快照历史
3. 再与远端同步

这意味着：

- `commit` 先于 `push`
- `fetch` 只拉取远端状态到本地仓库
- `pull` 会尝试把远端新状态推进当前分支

## 4. 环境准备

### 4.1 前提

当前工作区是一个 Rust workspace。CLI 位于 `crates/e2v-cli`。如果你在源码仓库里使用它，最直接的调用方式是：

```powershell
cargo run -p e2v-cli -- --help
```

当前 workspace 采用：

- Rust edition: `2024`
- CLI 参数解析：`clap`
- Web 服务：`axum`
- 远端后端能力：本地目录、S3、WebDAV、Alist

### 4.2 推荐执行方式

在开发或测试环境中，推荐始终用 `cargo run`：

```powershell
cargo run -p e2v-cli -- init .\my-repo --password "correct horse battery staple"
```

如果你已经单独构建了 CLI 二进制，也可以直接运行 `target\debug\e2v-cli.exe` 或发布版本二进制。

### 4.3 查看帮助

顶层帮助：

```powershell
cargo run -p e2v-cli -- --help
```

子命令帮助：

```powershell
cargo run -p e2v-cli -- push --help
cargo run -p e2v-cli -- verify --help
cargo run -p e2v-cli -- historical-rewrite --help
```

## 5. 快速开始

下面是一条最小可用工作流，适合第一次上手。

### 5.1 创建仓库

```powershell
New-Item -ItemType Directory -Path .\demo-repo | Out-Null
cargo run -p e2v-cli -- init .\demo-repo --password "correct horse battery staple"
```

预期结果：

- 仓库目录被初始化
- 当前默认分支为 `main`
- 仓库控制目录 `.e2v` 被创建

### 5.2 写入内容并提交

```powershell
Set-Content -Path .\demo-repo\notes.txt -Value "hello psyc-rust"
cargo run -p e2v-cli -- commit --repo .\demo-repo --message "first snapshot"
```

预期结果：

- 输出 `committed ...`
- 生成一个新的快照 ID

### 5.3 查看快照历史

```powershell
cargo run -p e2v-cli -- snapshots --repo .\demo-repo
```

预期结果：

- 能看到刚才的 `snapshot_id`
- 能看到提交消息 `first snapshot`

### 5.4 检出快照到独立目录

```powershell
New-Item -ItemType Directory -Path .\checkout-out | Out-Null
cargo run -p e2v-cli -- checkout --repo .\demo-repo --snapshot <SNAPSHOT_ID> --target .\checkout-out
```

这会把指定快照的内容物化到 `.\checkout-out`，不会替换你原始仓库目录。

## 6. 初始化与本地版本管理

### 6.1 `init`

用法：

```powershell
cargo run -p e2v-cli -- init <REPO> --password <PASSWORD> [--branch <BRANCH>]
```

示例：

```powershell
cargo run -p e2v-cli -- init .\repo-a --password "correct horse battery staple"
cargo run -p e2v-cli -- init .\repo-b --password "another secret" --branch trunk
```

说明：

- `REPO` 是仓库根目录。
- `--password` 是必填项。
- `--branch` 可选，默认是 `main`。

建议：

- 把初始化密码视为高价值秘密保存。
- 不要把密码写进共享脚本或日志。
- 如果你后面要启用共享和多设备，密码管理要更谨慎。

### 6.2 `commit`

用法：

```powershell
cargo run -p e2v-cli -- commit --repo <REPO> --message <MESSAGE>
```

示例：

```powershell
cargo run -p e2v-cli -- commit --repo .\demo-repo --message "seed"
```

行为说明：

- 读取当前仓库工作目录内容
- 生成新的快照
- 输出短格式的快照标识前缀

实践建议：

- 用能描述含义的提交消息，而不是 `update`、`fix1` 之类模糊文本。
- 在做远端同步前先 `commit`，避免未提交工作状态和远端状态混杂。

### 6.3 `snapshots`

用法：

```powershell
cargo run -p e2v-cli -- snapshots --repo <REPO>
```

示例输出通常类似：

```text
<snapshot_id> first snapshot
<snapshot_id> seed
```

用途：

- 查看当前仓库的快照历史
- 获取 `checkout` 或验证操作需要的 `snapshot_id`

### 6.4 `checkout`

用法：

```powershell
cargo run -p e2v-cli -- checkout --repo <REPO> --snapshot <SNAPSHOT> --target <TARGET_DIR>
```

说明：

- `checkout` 会把指定快照还原到一个目标目录。
- 这不是 Git 那种“切换当前工作副本到历史版本”的行为；这里更像“导出快照内容”。

实践建议：

- 为每次检出使用新的空目录，避免和现有文件冲突。
- 如果 `TARGET_DIR` 中已有内容，先备份重要文件。

## 7. 分支管理

### 7.1 查看分支

```powershell
cargo run -p e2v-cli -- branch --repo .\demo-repo list
```

输出特点：

- 当前分支前面会有 `*`
- 可能显示分支名和其头部快照 ID

### 7.2 创建分支

```powershell
cargo run -p e2v-cli -- branch --repo .\demo-repo create feature
```

### 7.3 切换分支

```powershell
cargo run -p e2v-cli -- branch --repo .\demo-repo checkout feature
```

### 7.4 删除分支

```powershell
cargo run -p e2v-cli -- branch --repo .\demo-repo delete feature
```

注意事项：

- 不要在仍需要保留的历史分支上直接删除。
- 先确认你当前不在该分支上。

推荐工作流：

1. 在 `main` 上准备基础状态
2. `branch create <name>`
3. `branch checkout <name>`
4. 在该分支上继续 `commit`

## 8. 搜索与只读浏览

### 8.1 `search`

用法：

```powershell
cargo run -p e2v-cli -- search <QUERY> --repo <REPO>
```

示例：

```powershell
cargo run -p e2v-cli -- search notes --repo .\demo-repo
```

当前行为：

- 优先匹配文件名
- 如果文件名没命中，会退化为一类元数据搜索

适用场景：

- 快速定位某个文件名
- 在快照索引中做轻量级内容组织查询

### 8.2 `serve`

用法：

```powershell
cargo run -p e2v-cli -- serve --repo <REPO>
```

作用：

- 启动一个本地 Web 只读服务
- CLI 会打印本地地址，通常是 `http://127.0.0.1:<port>/` 或类似 `localhost` 地址

示例：

```powershell
cargo run -p e2v-cli -- serve --repo .\demo-repo
```

适合：

- 在浏览器中查看快照和目录树
- 临时给本机用户做只读浏览

注意事项：

- 这是本地服务，不是公网托管能力。
- 服务启动后进程会保持运行，直到你手动停止。

## 9. 远端配置与同步

### 9.1 远端类型概览

当前用户层面最常见的远端包括：

- 本地目录远端：`file:///...`
- S3 兼容远端：`s3+https://...`
- WebDAV 远端：`webdav+https://...`
- Alist 远端：`alist+https://...`

基于测试中已经验证的格式，示例如下。

本地目录：

```text
file:///C:/e2v-remote
```

S3：

```text
s3+https://alice:secret@s3.example.com/example-bucket/sync-root?region=us-east-1
```

WebDAV：

```text
webdav+https://alice:secret@example.com/repo
```

Alist：

```text
alist+https://token@example.com/remote-root
```

注意：

- 文档中的账号、密码、token 都只是格式示例。
- 真实环境里不要把凭据硬编码到脚本、聊天记录或工单里。

### 9.2 添加默认远端

用法：

```powershell
cargo run -p e2v-cli -- remote --repo <REPO> add <NAME> <URL>
```

示例：

```powershell
New-Item -ItemType Directory -Path .\remote-store | Out-Null
cargo run -p e2v-cli -- remote --repo .\demo-repo add origin file:///C:/remote-store
```

说明：

- `NAME` 常用 `origin`
- `URL` 是远端规范字符串
- 配置完成后，该仓库的默认远端就建立了

### 9.3 `push`

用法：

```powershell
cargo run -p e2v-cli -- push --repo <REPO>
```

前提：

- 仓库已经存在默认远端
- 当前分支上已经有至少一个本地快照

行为：

- 将当前分支的头部状态发布到默认远端
- 输出被发布的快照前缀

典型工作流：

```powershell
Set-Content -Path .\demo-repo\notes.txt -Value "version 2"
cargo run -p e2v-cli -- commit --repo .\demo-repo --message "update notes"
cargo run -p e2v-cli -- push --repo .\demo-repo
```

### 9.4 `fetch`

用法：

```powershell
cargo run -p e2v-cli -- fetch --repo <REPO> [--password <PASSWORD>]
```

说明：

- `fetch` 从默认远端下载对象和分支状态到本地仓库
- 它不会像 `pull` 那样直接推进当前工作分支到新头部
- 某些场景下需要 `--password` 以便解锁或处理密钥相关状态

示例：

```powershell
cargo run -p e2v-cli -- fetch --repo .\demo-repo --password "correct horse battery staple"
```

### 9.5 `pull`

用法：

```powershell
cargo run -p e2v-cli -- pull --repo <REPO> [--password <PASSWORD>]
```

说明：

- `pull` 会尝试把默认远端的新状态并入当前本地分支
- 如果只是快进式更新，通常会成功
- 如果本地和远端已经分叉，命令会拒绝静默覆盖

示例：

```powershell
cargo run -p e2v-cli -- pull --repo .\demo-repo --password "correct horse battery staple"
```

重要行为边界：

- 如果本地有自己的新提交，而远端也有新的不兼容头部，`pull` 会报 diverged 或 conflict 类错误。
- 出现这种情况时，不要假设系统会自动帮你合并。

### 9.6 `clone`

用法：

```powershell
cargo run -p e2v-cli -- clone <REMOTE_SPEC> <TARGET_REPO_ROOT> --password <PASSWORD> --branch-token <BRANCH_TOKEN>
```

示例：

```powershell
cargo run -p e2v-cli -- clone file:///C:/remote-store .\demo-clone --password "correct horse battery staple" --branch-token <BRANCH_TOKEN>
```

说明：

- `clone` 需要显式指定远端和目标目录
- 还需要指定远端上要跟踪的 `branch-token`

如果你不知道 `branch-token`：

- 它通常来自拥有该仓库的一端
- 当前 CLI 主要把它当作同步标识，而不是像 Git 那样只凭分支名工作

## 10. 验证、诊断与修复

### 10.1 `verify snapshot`

用法：

```powershell
cargo run -p e2v-cli -- verify --repo <REPO> snapshot <SNAPSHOT_ID>
```

作用：

- 验证指定快照图是否完整、可读、未损坏

### 10.2 `verify object`

用法：

```powershell
cargo run -p e2v-cli -- verify --repo <REPO> object <EXPECTED_TYPE> <OBJECT_ID>
```

示例：

```powershell
cargo run -p e2v-cli -- verify --repo .\demo-repo object snapshot <SNAPSHOT_ID>
```

适合：

- 调查单个对象损坏
- 对异常对象做精确排查

### 10.3 `verify remote`

用法：

```powershell
cargo run -p e2v-cli -- verify --repo <REPO> remote --sample <SAMPLE_PERCENT>
```

示例：

```powershell
cargo run -p e2v-cli -- verify --repo .\demo-repo remote --sample 100%
```

说明：

- 会对默认远端做抽样验证
- `100%` 表示最全面但也最重
- 输出通常包含采样对象数量和本地修复统计

建议：

- 小仓库可直接用 `100%`
- 大仓库可先低比例采样，再在维护窗口里做更重验证

### 10.4 `repair`

用法：

```powershell
cargo run -p e2v-cli -- repair --repo <REPO>
```

或在危险场景下：

```powershell
cargo run -p e2v-cli -- repair --repo <REPO> --force-accept-remote-rollback --confirm-remote-rollback --password <PASSWORD>
```

普通修复模式的用途：

- 当本地对象损坏或缺失时，尝试从默认远端修复

危险模式的用途：

- 明确接受远端回滚，并以远端状态重建本地事实视图

危险模式注意事项：

- 这是强干预动作
- 必须同时提供 `--force-accept-remote-rollback`
- 还必须提供第二重确认 `--confirm-remote-rollback`
- 并提供 `--password`

如果缺少第二重确认，命令会拒绝执行。

### 10.5 `doctor`

用法：

```powershell
cargo run -p e2v-cli -- doctor --repo <REPO>
```

生成打包诊断信息：

```powershell
cargo run -p e2v-cli -- doctor --repo <REPO> --bundle .\doctor-bundle
```

作用：

- 输出远端与可信状态摘要
- 检查是否支持 GC 执行
- 在 `--bundle` 模式下写出一组诊断文件

当前测试验证过的诊断产物包括：

- `doctor-summary.json`
- `trusted-state.json`

安全特性：

- 诊断 bundle 会对本地路径、`file:///` 远端路径、S3 凭据等敏感信息做脱敏

适合场景：

- 排障
- 向团队成员共享诊断材料
- 在不暴露敏感路径或密钥的前提下留存问题快照

## 11. 共享协作

共享功能允许你按成员和设备进行授权管理。

### 11.1 查看当前共享状态

```powershell
cargo run -p e2v-cli -- share --repo .\demo-repo list
```

输出通常包括：

- actor 记录
- device 记录

### 11.2 邀请成员

```powershell
cargo run -p e2v-cli -- share --repo .\demo-repo invite-member --name Alice --out .\alice-member-invite.bin
```

说明：

- 会生成一个邀请 bundle 文件
- 该文件应通过安全方式传递给目标成员

### 11.3 接受成员邀请

```powershell
cargo run -p e2v-cli -- share --repo .\demo-repo accept-member --bundle .\alice-member-invite.bin --label alice-laptop
```

### 11.4 邀请设备

```powershell
cargo run -p e2v-cli -- share --repo .\demo-repo invite-device --actor <ACTOR_ID> --label alice-phone --out .\alice-device-invite.bin
```

### 11.5 接受设备邀请

```powershell
cargo run -p e2v-cli -- share --repo .\demo-repo accept-device --bundle .\alice-device-invite.bin --label alice-phone
```

### 11.6 撤销成员

```powershell
cargo run -p e2v-cli -- share --repo .\demo-repo revoke-member --actor <ACTOR_ID> --password "correct horse battery staple"
```

### 11.7 撤销设备

```powershell
cargo run -p e2v-cli -- share --repo .\demo-repo revoke-device --device <DEVICE_ID> --password "correct horse battery staple"
```

共享安全建议：

- 邀请 bundle 应视为敏感文件
- 不要通过无保护的公开频道分发
- 成员和设备的撤销操作都应保留内部审批或操作记录

## 12. 高级运维

这一章介绍更强、更重、也更需要谨慎操作的命令。

### 12.1 `gc`

`gc` 用于远端垃圾回收分析和执行。

#### 12.1.1 Dry Run

```powershell
cargo run -p e2v-cli -- gc --repo .\demo-repo --dry-run
```

作用：

- 列出不可达的远端物理引用数量
- 查看当前活动意图等状态

适合：

- 正式删除前做评估
- 维护窗口前的预检查

#### 12.1.2 Execute

```powershell
cargo run -p e2v-cli -- gc --repo .\demo-repo --execute --grace-period 30d --confirm-single-writer-maintenance-window
```

说明：

- `--grace-period` 是必填
- 当前支持像 `30d` 这样的格式
- 对单写者远端，执行删除前需要显式确认维护窗口

为什么需要确认：

- 真正删除远端物理引用会影响恢复空间
- 对单写者模型，建议在明确维护窗口内操作

如果不加 `--confirm-single-writer-maintenance-window`，命令会拒绝执行并提示 maintenance window 相关信息。

### 12.2 `historical-rewrite`

这是高风险安全运维命令，用于做历史强撤销和全历史重写。

#### 12.2.1 查看计划

```powershell
cargo run -p e2v-cli -- historical-rewrite --repo .\demo-repo plan
```

当前输出会包含类似信息：

- `historical strong revocation plan`
- reachable objects
- remote loose objects
- remote packed objects
- old epochs
- advisory 提示

适用场景：

- 成员撤销后需要处理历史可达对象
- 仓库进入更强的历史保护流程

#### 12.2.2 执行重写

```powershell
cargo run -p e2v-cli -- historical-rewrite --repo .\demo-repo execute --password "correct horse battery staple" --confirm-full-reencryption
```

说明：

- `--password` 必填
- `--confirm-full-reencryption` 必填
- 没有第二重确认时命令会拒绝执行

风险提示：

- 这不是普通同步命令
- 它可能重写可达历史、退休旧 epoch，并留下后续需要 GC 的远端陈旧引用
- 执行前应先跑 `plan`
- 强烈建议先做仓库和远端备份

### 12.3 `oram`

`oram` 命令用于访问模式隐藏相关布局能力。

#### 12.3.1 查看计划

```powershell
cargo run -p e2v-cli -- oram --repo .\demo-repo plan
```

输出通常包含：

- `oblivious layout plan`
- real reads
- cover reads
- bytes per request
- write amplification

#### 12.3.2 查看状态

```powershell
cargo run -p e2v-cli -- oram --repo .\demo-repo status
```

可查看：

- 当前布局模式
- dedup 模式
- layout generation
- oblivious generation
- policy

#### 12.3.3 启用

```powershell
cargo run -p e2v-cli -- oram --repo .\demo-repo enable --policy balanced
```

#### 12.3.4 重洗牌

```powershell
cargo run -p e2v-cli -- oram --repo .\demo-repo reshuffle --policy balanced
```

使用建议：

- 在理解代价前不要贸然启用
- `plan` 先于 `enable`
- 对大仓库，关注读放大、写放大和维护成本

## 13. 推荐工作流

### 13.1 单机日常使用

```powershell
cargo run -p e2v-cli -- init .\repo --password "correct horse battery staple"
Set-Content -Path .\repo\notes.txt -Value "v1"
cargo run -p e2v-cli -- commit --repo .\repo --message "seed"
cargo run -p e2v-cli -- snapshots --repo .\repo
```

### 13.2 配置默认远端并发布

```powershell
New-Item -ItemType Directory -Path C:\remote-store | Out-Null
cargo run -p e2v-cli -- remote --repo .\repo add origin file:///C:/remote-store
cargo run -p e2v-cli -- push --repo .\repo
```

### 13.3 第二台机器克隆并同步

```powershell
cargo run -p e2v-cli -- clone file:///C:/remote-store .\repo-clone --password "correct horse battery staple" --branch-token <BRANCH_TOKEN>
cargo run -p e2v-cli -- remote --repo .\repo-clone add origin file:///C:/remote-store
cargo run -p e2v-cli -- fetch --repo .\repo-clone --password "correct horse battery staple"
cargo run -p e2v-cli -- pull --repo .\repo-clone --password "correct horse battery staple"
```

### 13.4 周期性健康检查

```powershell
cargo run -p e2v-cli -- verify --repo .\repo remote --sample 100%
cargo run -p e2v-cli -- doctor --repo .\repo --bundle .\doctor-out
cargo run -p e2v-cli -- gc --repo .\repo --dry-run
```

## 14. 常见问题与排障

### 14.1 `pull` 报 diverged 或 conflict

含义：

- 本地和远端都有各自新增历史，系统拒绝静默推进

建议：

- 先确认哪一侧应保留
- 不要直接做危险修复
- 先用 `verify`、`doctor` 看状态

### 14.2 `repair` 要求二次确认

这通常说明你正在请求一个危险操作，比如接受远端回滚。系统这样设计是为了避免单个误触参数就破坏本地状态。

### 14.3 `gc --execute` 报 maintenance window

说明：

- 当前远端能力模型要求你显式确认单写者维护窗口

做法：

- 先跑 `--dry-run`
- 安排维护窗口
- 再加 `--confirm-single-writer-maintenance-window`

### 14.4 `historical-rewrite execute` 不接受执行

如果没有 `--confirm-full-reencryption`，命令会拒绝。这是预期行为。

### 14.5 `doctor` bundle 是否会泄露路径或凭据

按当前测试覆盖，bundle 会对：

- 本地仓库路径
- 本地目录远端路径
- `file:///` URL
- S3 凭据和桶名

做脱敏处理。但你仍应把诊断 bundle 当作内部资料对待。

## 15. 安全建议

- 把初始化密码保存在安全的凭据管理器中。
- 不要把远端凭据明文写入公开脚本。
- 邀请 bundle、诊断 bundle 都应按敏感资料处理。
- 任何带确认开关的危险命令，都应先做 `plan`、`verify`、`doctor`。
- 对 `historical-rewrite`、`gc --execute`、强制回滚接受等动作，建议先离线备份。

## 16. 命令速查

### 16.1 基础命令

```powershell
cargo run -p e2v-cli -- init <REPO> --password <PASSWORD> [--branch <BRANCH>]
cargo run -p e2v-cli -- commit --repo <REPO> --message <MESSAGE>
cargo run -p e2v-cli -- snapshots --repo <REPO>
cargo run -p e2v-cli -- checkout --repo <REPO> --snapshot <SNAPSHOT> --target <TARGET_DIR>
cargo run -p e2v-cli -- branch --repo <REPO> list
cargo run -p e2v-cli -- branch --repo <REPO> create <NAME>
cargo run -p e2v-cli -- branch --repo <REPO> checkout <NAME>
cargo run -p e2v-cli -- branch --repo <REPO> delete <NAME>
cargo run -p e2v-cli -- search <QUERY> --repo <REPO>
```

### 16.2 同步命令

```powershell
cargo run -p e2v-cli -- remote --repo <REPO> add <NAME> <URL>
cargo run -p e2v-cli -- push --repo <REPO>
cargo run -p e2v-cli -- fetch --repo <REPO> [--password <PASSWORD>]
cargo run -p e2v-cli -- pull --repo <REPO> [--password <PASSWORD>]
cargo run -p e2v-cli -- clone <REMOTE_SPEC> <TARGET_REPO_ROOT> --password <PASSWORD> --branch-token <BRANCH_TOKEN>
```

### 16.3 共享命令

```powershell
cargo run -p e2v-cli -- share --repo <REPO> list
cargo run -p e2v-cli -- share --repo <REPO> invite-member --name <NAME> --out <OUT>
cargo run -p e2v-cli -- share --repo <REPO> accept-member --bundle <BUNDLE> --label <LABEL>
cargo run -p e2v-cli -- share --repo <REPO> invite-device --actor <ACTOR> --label <LABEL> --out <OUT>
cargo run -p e2v-cli -- share --repo <REPO> accept-device --bundle <BUNDLE> --label <LABEL>
cargo run -p e2v-cli -- share --repo <REPO> revoke-member --actor <ACTOR> --password <PASSWORD>
cargo run -p e2v-cli -- share --repo <REPO> revoke-device --device <DEVICE> --password <PASSWORD>
```

### 16.4 验证与运维命令

```powershell
cargo run -p e2v-cli -- verify --repo <REPO> snapshot <SNAPSHOT_ID>
cargo run -p e2v-cli -- verify --repo <REPO> object <EXPECTED_TYPE> <OBJECT_ID>
cargo run -p e2v-cli -- verify --repo <REPO> remote --sample <SAMPLE_PERCENT>
cargo run -p e2v-cli -- repair --repo <REPO>
cargo run -p e2v-cli -- repair --repo <REPO> --force-accept-remote-rollback --confirm-remote-rollback --password <PASSWORD>
cargo run -p e2v-cli -- doctor --repo <REPO> [--bundle <BUNDLE>]
cargo run -p e2v-cli -- gc --repo <REPO> --dry-run
cargo run -p e2v-cli -- gc --repo <REPO> --execute --grace-period <DAYS> --confirm-single-writer-maintenance-window
cargo run -p e2v-cli -- historical-rewrite --repo <REPO> plan
cargo run -p e2v-cli -- historical-rewrite --repo <REPO> execute --password <PASSWORD> --confirm-full-reencryption
cargo run -p e2v-cli -- oram --repo <REPO> plan
cargo run -p e2v-cli -- oram --repo <REPO> status
cargo run -p e2v-cli -- oram --repo <REPO> enable --policy balanced
cargo run -p e2v-cli -- oram --repo <REPO> reshuffle --policy balanced
cargo run -p e2v-cli -- serve --repo <REPO>
```

## 17. 最后建议

如果你刚开始使用这个项目，推荐按这个顺序掌握：

1. `init`、`commit`、`snapshots`
2. `checkout`、`branch`
3. `remote add`、`push`、`fetch`、`pull`、`clone`
4. `verify`、`repair`、`doctor`
5. `gc`、`historical-rewrite`、`oram`

把高级运维命令留到你已经熟悉普通同步和恢复流程之后再使用，会更安全。
