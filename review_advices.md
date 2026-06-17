## reviewer1

### 1. 密码学平面：Epoch 密钥派生导致撤销机制形同虚设

在 **4.4 Repo Master Key、Epoch 与子密钥** 以及 **4.5 Keyring** 的设计中，存在严重的密码学逻辑矛盾，会导致 P2/P3 的设备撤销与强撤销失效。

* **缺陷描述：** 规格书中规定用**一个**随机仓库主密钥（`repo_master_key`）配合 `epoch` 派生所有子密钥。当执行设备撤销时，推进 `active_epoch`，新对象使用新 epoch 加密。然而，被撤销的设备在其本地已经持有了那个单一的 `repo_master_key`。即使你删除了远端 Keyring 中属于该设备的 envelope，该设备依然可以用手中保留的 `repo_master_key` 加上新的 epoch 编号，自行推导出撤销后的 `repo_object_id_key` 等子密钥，从而继续解密未来产生的数据。
* **修复方案：**
* 不能使用单一的 `repo_master_key` 去 Hash 不同的 epoch。必须采用 **独立 Epoch Root Key** 的设计。
* Keyring 的每个 Envelope 内部加密的不是单一主密钥，而是一个**密钥加密密钥（KEK, Key Encrypting Key）**。
* 引入一个统一的 Epoch Key Store（作为 Keyring 的一部分或独立对象），里面存放 `Epoch 1 Root Key`, `Epoch 2 Root Key`... 每个 Root Key 由当前合法的 KEK 加密。
* 撤销时：生成新的 `Epoch N Root Key`，同时轮转 KEK，用新 KEK 加密新的 Epoch 列表，并只为未撤销的设备生成新 KEK 的 envelope。



### 2. 元数据平面：哈希分片（Tree Sharding）摧毁大目录的遍历与呈现

在 **5.5 Manifest Store** 中关于 Tree Sharding 的设计，与 **7.4 Local Web UI / 7.6 VFS** 强依赖的目录浏览需求存在不可调和的冲突。

* **缺陷描述：** 规格书定义大目录的分片键为 `shard_key = BLAKE3_keyed(..., normalized_entry_name)`。哈希是完全无序的散列函数。如果一个目录下有 100 万个文件，被分散到 256 个 `TreeShard` 中，当 VFS 或 Web UI 需要按字母顺序分页展示前 100 个文件（例如 `ls -l` 或网页加载第一页）时，由于字母 `A` 开头的文件被随机哈希到了全部 256 个分片中，系统**必须先下载、解密并解析全部 256 个分片**，将 100 万条记录载入内存重新排序，才能返回前 100 个结果。这会导致严重的读放大和内存爆炸，直接击穿 N+1 控制和 ReadService 的性能指标。
* **修复方案：**
* 放弃基于 Hash 的目录分片，改用 **Lexicographical Range Sharding（基于字典序的前缀范围分片，类似 B-Tree 结构）**。
* 例如：`TreeShard(range="a"-"c")`，`TreeShard(range="d"-"f")`。
* 由于 `entry_name` 在 Tree 对象中是加密存储的，远端依然无法知道具体的明文文件名，但本地 ReadService 在获取目录第一页时，只需下载并解密第一个 Range Shard，完美契合流式读取和分页加载。



### 3. 数据与同步平面：流式流水线与断点续传状态机的悖论

在 **3.7 Stream Pipeline** 和 **6.12/6.13 Operation Journal 断点续传** 的结合处，存在状态机无法闭环的死锁缺陷。

* **缺陷描述：** 规格书中 6.13 恢复流程描述为：“page through object_uploads where state != verified... stat uploaded objects... exists planned objects -> upload missing”。这隐含了一个前提：在上传前，Journal 中已经存有了所有 Chunk 的 `planned` 状态和 `object_id`。
但对于一个 1TB 的大文件，按照 3.7 的 **Stream Pipeline (reader -> chunker -> hasher -> encryptor -> uploader)** 边读边切边传的设计（为了控制内存，即 One-Pass 架构），系统在流式处理中断前，**根本无法预知尚未读取的后半部分会产生哪些 `object_id**`。因此，崩溃后 Journal 里只有已经传完的记录，没有未传部分的 `planned` 记录。按照原恢复逻辑，程序将无法知道缺失了哪些对象，进而无法续传。
* **修复方案：**
* 修改断点续传语义。对于单体大文件的 Resume，不应依赖于事先写入的 `planned` 清单。
* 恢复逻辑必须是：**重新执行 Stream Pipeline**（即重新读取文件、重新切块、重新 Hash），但在 `hasher` 计算出 `object_id` 准备交给 `encryptor` 之前，先拦截并查询 Operation Journal 或 Local Index。如果该 ID 已存在且状态为 `verified`，则跳过加密和上传步骤，直接推入下一阶段。这就把 Resume 变成了“跳过已知块的快速重放”，保全了流式架构的内存约束。



### 4. 运行时平面：Tokio 与 CPU 密集任务的混合背压极易死锁

在 **8.3 Scheduler** 和 **8.4 Backpressure** 针对 Rust 工程的调度设计中，存在实现层面的隐患。

* **缺陷描述：** 规格要求“有界队列”、“CPU 池不得同步等待 Tokio 任务释放容量”、“背压必须逐级传播”。在 Rust 中，如果你使用 `rayon` 处理 chunk/hash，并通过具有固定容量（bounded channel）的队列将结果发回 Tokio 的 uploader，一旦上传网络限流，队列打满，`rayon` 线程在尝试 `send()` 时会被阻塞。如果 `rayon` 的全局线程池被全部阻塞在等待 Tokio 队列释放，而 Tokio 又恰好需要发起一些计算任务丢给 `rayon` 才能继续完成当前的上传清理工作，就会发生**典型的跨运行时死锁**。
* **修复方案：**
* 在技术选型中，明确禁止对属于核心流水线的 `hash/encrypt/chunking` 使用共享的 `rayon` 全局线程池。
* 必须使用专属的同步工作池（如独立的 `ThreadPool` 或通过限制容量的 `tokio::task::spawn_blocking` 封装），并在 channel 满时，允许上游主动让出执行权（yield）或使用非阻塞 `try_send` 配合自旋/退避，确保不会因为背压导致整个线程池资源被永久占据。



### 5. 并发存储：Pack Index Compaction 的隐性写冲突

在 **3.8 Pack Writer** 和 **6.8 TransactionPublisher** 中关于 Pack Index 的设计存在一个竞态盲区。

* **缺陷描述：** 规范提出 P1 实现“Pack Index Compaction”。当 Client A 觉得远端 `level0` 的 Index Segments 太多了，在本地将其合并为 `level1` 并准备发布新的 Index Root。但此时，Client B 刚好完成了一个新 Pack 的上传，并且也在尝试 CAS 更新 Index Root 以追加它的 `level0` segment。
如果 Client A 的 Compaction 抢先 CAS 成功，Client B 的 CAS 会失败。根据常规重试逻辑，Client B 会重新拉取 Index Root，把自己的 segment 追加进去再次 CAS。这看似没有问题。
但如果 **Client B 抢先 CAS 成功**，Client A 拉取到新的 Index Root，此时 Client A 原本打包好的 Compaction Segment 就少包含了 Client B 刚刚提交的 segment，如果 Client A 直接重试 CAS，就会**抹掉或遗漏** Client B 刚发布的 Index 引用。
* **修复方案：**
* 在 `TransactionPublisher` 的协议里明确：如果 Compaction 触发的 CAS 失败，**必须丢弃或重新合并**当前已生成的 Compaction Segment，不允许直接 Rebase Index Root。或者，Compaction 只能作为单一写者锁下的离线维护任务（`e2v repack`），不允许在多客户端的普通 Push 流程中作为后台任务异步触发。


## reviewer2

### 1. VFS 读放大雪崩与 AEAD 认证边界的冲突

在 **7.6 VFS** 与 **3.6 Encryptor** 的结合处，存在严重的性能与内存矛盾，这在浏览大型数据集或数据库文件时会极其致命。

* **缺陷描述：** 规范规定“单个 chunk 必须作为最小认证单元... VFS 和 HTTP Range 只能返回已经完成认证的 chunk”。同时，默认的平均 chunk size 是 1 MiB。对于不支持可靠 Invalidation 的系统，规范建议 VFS 使用 Direct I/O（关闭内核 Page Cache）。
假设用户通过 VFS 挂载仓库，并用工具读取一个 100GB 归档文件中的某个 4KB 局部数据（例如读取 ZIP 目录树或 SQLite 索引页）。操作系统发起 4KB 的 `read()` 请求，ReadService 为了完成 AEAD 验证，**必须从远端拉取 1 MiB 数据，全部解密并验证 Auth Tag，然后丢弃 99.6% 的数据，只返回 4KB**。
由于关闭了内核缓存，如果应用端发起连续的随机小块读取，会引发指数级的读放大（Read Amplification）和 CPU 飙升。而如果完全依赖内部的“进程内 LRU 明文缓存”，在 Rust 的异步并发模型下，如果锁粒度设计不当，极其容易引发缓存击穿和内存溢出。
* **修复建议：**
* 必须在 Encryptor 层面引入 **Sub-chunk Authentication（块内子段认证）** 机制。例如，即使物理 Chunk 是 1 MiB，内部也应该按 64 KiB 或更小粒度切分并独立附加 MAC/Tag。
* 这样 Range Read 只需要拉取并验证包含目标 4KB 的那个 64 KiB 子段，将读放大控制在可接受范围内。



### 2. 单一写者租约（Single-Writer Lease）的死锁与“僵尸意图”

在 **6.9 Backend Capability** 和 **6.11 Sync Engine (Push/GC)** 中，单写者模式的容灾设计存在逻辑死结。

* **缺陷描述：** 规范指出，对于不支持 CAS 的后端（如某些 WebDAV），只能使用 `single-writer` 模式，且依赖远端 Lease。同时规定，Write Intent 存在时（默认 `intent_expiry` 可能长达 72 小时），会阻止破坏性操作。
假设设备 A（笔记本）正在向 WebDAV 执行大规模 Push，获取了 Lease 并在远端写入了 Intent，但中途断网或内核崩溃（没有机会释放 Lease）。
此时设备 B（台式机）尝试 Push，会发现远端存在活动 Lease。规范规定“无 CAS 后端禁止后台自动 push；用户显式 `--force-single-writer-risk` 只能用于人工恢复”。这意味着在长达数十小时的 Lease 超时前，设备 B 被**完全锁死**。如果用户不耐烦使用了 `--force` 进行接管，而此时设备 A 突然恢复网络并继续用旧状态盲写（由于缺乏 CAS，后端无法拒绝 A 的写入），整个仓库的 Manifest 将瞬间损坏。
* **修复建议：**
* 不能仅靠超时，必须引入 **Fencing Token（隔离令牌或世代号）** 机制。
* 即便是单写者后端，Lease 也必须包含一个单调递增的 `Lease Generation`。强制接管时，设备 B 会覆写 Lease 并提升 Generation。
* 设备 A 在上传任何对象前，必须低成本重读一遍 Lease Generation。如果发现自己的 Token 已过期，立刻自我熔断（Fencing），从而在无 CAS 限制下最大程度防止脑裂。



### 3. 活锁（Livelock）危机：活跃大文件的 Torn Read 保护失效

在 **3.3 File Scanner** 的源文件一致性策略中，处理长时间读取对象的机制在无快照环境下必定触发活锁。

* **缺陷描述：** 规范要求：“如果读后校验发现文件变化，必须丢弃该文件本次产生的临时 chunk，并重试”。
考虑一个正在不断追加日志的 5GB 文件，或者正在跑训练任务动态更新的权重文件。Chunker 处理这 5GB 数据需要几分钟。当读取完成时，文件 `mtime` 或大小已经发生变化。系统按规范丢弃数据并重试。由于该文件是活跃的，下一次读取完成后依然会发现变化。这会导致 `e2v commit` 陷入**无限重试的活锁循环**，永远无法完成当前 Snapshot 的提交。
* **修复建议：**
* 对于无法利用 OS 级快照（VSS/LVM/APFS Snapshot）的平台，必须引入重试阈值（如 `max_volatile_retries = 3`）。
* 到达阈值后，系统必须提供明确的后备策略：要么跳过该文件（Skip with Warning），要么将其标记为 `Dirty/Unstable` 并强制打包入库（用户如果只想要一个 Crash-Consistent 的状态，这是合理的）。绝对不能无限期阻塞整个工作树的提交。



### 4. 密码学悖论：ORAM (P3) 与 CDC 去重 (P0) 的理论互斥

在 **P3 访问模式隐藏 (ORAM)** 与 **3.5 Hasher / CDC 去重** 的长期路线图中，存在密码学理论层面的根本矛盾。

* **缺陷描述：** P0 阶段的核心特性是 CDC（内容定义切块）与去重，这要求使用 **Deterministic Encryption（确定性加密/收敛加密）**，即明文相同的 Chunk，必须生成完全相同的 Keyed Object ID 和密文（或至少 ID 必须相同以便复用）。
然而，P3 规划了 ORAM（Oblivious RAM）风格的布局来隐藏访问模式。ORAM 的核心安全要求是 **Semantic Security（语义安全，CPA 安全）**，它要求同一个数据块在不同时间被写入或访问时，表现出完全不可区分的随机化特征。
如果系统保留了基于内容的确定性 Object ID，那么即使你做了流量混淆（Traffic Shaping）和填充（Padding），远端存储和网络观察者依然能通过统计某个确定的 Object ID 被引用的频率，推断出数据的重合度、目录结构变化，甚至通过已知结构的公开数据集进行频率分析，从而轻易击穿 ORAM 的防御假设。
* **修复建议：**
* 在架构文档中必须明确声明：**ORAM (P3) 与全局/长期 CDC 去重是互斥的特性**。
* 当用户在 P3 开启 ORAM-style Storage Layout 时，Object ID 必须转为非确定性的随机 ID，彻底放弃同内容数据的物理去重；或者采用更昂贵的动态重洗（Reshuffle）协议，但这必然需要重写底层的 LogicalObjectStore 抽象模型，而不是简单的“后端扩展”。


## reviewer3


### 1. 致命缺陷：Epoch 轮转与 CDC 增量去重的数学互斥

在 **4.4 Repo Master Key、Epoch 与子密钥** 以及 **3.5 Hasher** 的结合中，存在一个密码学定义导致的灾难性后果。

* **缺陷描述：**
规范定义 `object_id = BLAKE3_keyed(repo_object_id_key, canonical_plaintext)`。
同时在 P2 阶段定义，设备撤销时会发生 Epoch 轮转，推进 `active_epoch`，从而产生**新的** `repo_object_id_key`。
**数学悖论来了：** 一旦 Epoch 轮转，同一个明文 Chunk（哪怕是 10 年前的文件，甚至是一个空文件），使用新的 Key 算出的 `object_id` 将与旧版本**完全不同**。
这意味着什么？如果你有一个 10TB 的仓库，仅仅因为你踢出了一个旧设备（触发了 Epoch 轮转），当你修改了一个 1KB 的文件并尝试 Push 时，Chunker 切出的所有未修改数据的哈希值全部变了！系统会认为这 10TB 数据在远端都不存在，从而**强制全量重新上传这 10TB 数据**。CDC 增量去重在跨越 Epoch 边界时彻底失效。
* **修复建议：**
* **对象 ID 派生必须与 Epoch 解耦**。`object_id` 的 keyed hash 不能使用随生命周期变化的密钥，必须使用一个建库时生成且**永远不可变**的 `repo_dedup_key`。
* 为了实现撤销，轮转的只能是**加密密钥**（`repo_chunk_enc_key`），而不是哈希密钥。旧设备或许能通过哈希碰撞猜出某些对象存在，但它没有新 Epoch 的解密密钥，依然无法读取新内容。



### 2. 致命缺陷：分布式 GC 的“老旧复用块”误删陷阱

在 **6.14 GC / Verify / Repair** 和 **6.11 Sync Engine (Write Intent)** 中，缺乏中心化锁机制的垃圾回收规则会误删正在被引用的健康数据。

* **缺陷描述：**
规范规定，GC 删除对象的依据是：对象无法从远端 Ref 触达，且其远端 `Last-Modified` 大于 `gc_safe_horizon`，且当前没有活动的 Write Intent 阻止 GC。
同时，出于隐私考虑，Write Intent 中**不记录**正在上传的具体 Object 列表。
考虑这个场景：客户端 A 在本地做了一个基于旧分支的 Commit（尚未 Push），引用了一个一年前上传的复用 Chunk（其远端 `Last-Modified` 是一年前）。客户端 A 准备 Push，由于网络原因遭遇 CAS 冲突失败，该 Snapshot 在本地变为 `needs-rebase` 状态。此时 A 的 Write Intent 到期失效。
客户端 B 运行了 `gc --execute`。由于 A 的新快照尚未挂载到远端 Ref，B 认为那个一年前的 Chunk 是“不可达”的，且它的物理时间远超 `gc_safe_horizon`，也没有活跃的 Intent 保护它，于是 **GC 合法地删除了这个 Chunk**。
此时，客户端 A 恢复网络，清理冲突并准备重新 rebase 发布，却发现自己依赖的历史 Chunk 已经被物理销毁，本地快照彻底损坏。
* **修复建议：**
* 不能仅依赖简单的远端时钟判断旧块的生死。任何针对无主/游离 Snapshot（即使在本地）的保护，都必须依赖一套**对象租约或心跳刷新机制**。
* 或者，强制要求 Push 失败后进入 `needs-rebase` 状态的本地客户端，必须定期向远端续期某种 “Unpublished Keep-Alive” 标记；再或者，GC 扫描时不能单纯依赖 Chunk 自身的修改时间，而必须引入基于 Mark-and-Sweep 的两阶段标记法（甚至写时复制标记），但这会极大地增加弱后端的 I/O 成本。



### 3. 高风险：跨平台路径本地映射的“Clone 孤岛效应”

在 **3.3 File Scanner** 的路径策略中，关于 macOS/APFS 路径 read-back 规范化的设定存在断层。

* **缺陷描述：**
规范为了保证跨平台兼容，要求底层统一使用 NFC。如果 macOS 的 APFS 强行将其转换为 NFD 或发生等价变化，“客户端必须在本地 checkout metadata 中记录映射，该映射只属于本地工作区状态，不进入远端 manifest”。
这种“仅限本地”的处理会破坏 Clone 的确定性。假设我在 Mac A 上新建了一个带有特殊音标的文件，生成了本地映射，并提交到远端（远端保存为干净的 NFC）。随后，我的同事在 Mac B 上执行 `e2v clone`。
Mac B 刚拉下来代码进行 checkout 时，它**没有** Mac A 的本地映射表。当它试图验证检出文件时，APFS 再次转换了文件名，Mac B 的 scanner 扫描后会认为：远端的 NFC 文件被删除了，同时工作区多出了一个 NFD 的未追踪文件（Untracked File）。同事什么都没改，一 clone 下来 `status` 就是脏的。
* **修复建议：**
* 这种由于 OS 文件系统驱动底层特性导致的别名映射（Alias Mapping），不能仅仅作为本地抛弃状态。
* 必须在 `Object Model` 的 Commit Metadata 中引入一个非事实源的 `platform_hints` 字段，把这类已知的文件系统变形怪癖广播给同类操作系统的其他设备，作为 Checkout 时的校验容错凭证。



### 4. 架构悖论：P3 历史强撤销的“重加密成本”在海量数据下不可行

在 **4.5 Keyring** 和 **P3-A 历史强撤销** 路线图中，为了满足完美的前向保密（Forward Secrecy），设计了极度暴力的操作。

* **缺陷描述：**
规范提到：“P3 实现历史数据强撤销，必须通过全仓库重加密、旧 epoch 退休和对象重写 journal 完成”。
对于一个个人笔记仓库（几百 MB），这没问题。但系统定位也包括“大型二进制文件、模型权重、海量数据集”。假设这是一个 5TB 的团队研发仓库。仅仅因为一个外包员工离职，为了在理论上让他手中残留的旧 Epoch 密钥失效，你需要在一台本地机器上，把远端 5TB 的密文全部下载、解密、用新 Epoch 重新加密、再重新上传 5TB 的密文，并执行 Layout Rewrite。
这需要数天到数周的网络 I/O 和计算，且中间产生的天量流量费和 API 调用费将是灾难性的。在实际工程中，这种“强撤销”等于“不可用”。
* **修复建议：**
* 在架构规格中承认 **E2EE 环境下的 O(N) 历史重加密是不现实的**。
* 对于海量数据场景，不应在存储层强求数学意义上的历史对象撤销，而应转入**访问控制层（ACL/IAM）**。即：吊销该外包员工的远端存储读取凭证（如吊销 S3 临时 Token）。虽然他手里的密钥理论上能解开旧数据，但他已经碰不到物理密文了。未来的新数据再使用新的 Epoch 密钥加密即可（O(1) 成本）。必须将存储访问控制作为 E2EE 架构的合法补充，而不是试图用纯密码学硬抗物理定律。



## reviewer4 

### 1. 密码学“天坑”：随机 Padding 导致的 AES-GCM Nonce 重用灾难

在 **3.6 Encryptor** 的 Padding Policy 和 Nonce 派生设计中，存在一个会导致整个加密体系瞬间崩塌的密码学漏洞。

* **缺陷描述：**
规范规定 `nonce = first_96_bits(BLAKE3_keyed(repo_nonce_key, object_id || object_type || version))`。
同时，为了隐藏目录规模，规范支持 `randomized-manifest-padding`。
**致命逻辑链：** 假设系统需要重新上传一个 Manifest（例如因为网络中断重试，或者强制覆盖）。它的 `object_id` 由规范化的明文（Canonical Plaintext，不含 Padding）计算而来，因此 `object_id` 是固定的。那么按照上述公式，派生出的 `nonce` 也是**固定**的。
但是！由于 Padding 是**随机**的，每次加密前，最终的明文载荷（Payload = Plaintext + Random Padding）是**不同**的。
在 AES-GCM 中，**使用相同的 Key 和相同的 Nonce 加密两段不同的明文，是绝对的密码学禁忌**。这会直接导致攻击者可以通过异或密文，反推出 GCM 的认证密钥（Authentication Key $H$），从而不仅能伪造后续的所有数据，还能轻易破解密文。
* **修复方案：**
* **Nonce 派生必须包含所有可能改变最终密文的变量**。公式必须修改为：`nonce = first_96_bits(BLAKE3_keyed(repo_nonce_key, object_id || object_type || version || padding_policy || padding_seed))`。
* 或者采用更安全的方案：使用扩展 Nonce 长度的 AEAD 算法（如优先推荐的 **XChaCha20-Poly1305**，具有 192-bit 的大 Nonce），放弃确定性 Nonce 派生，每次加密直接生成安全的 `random_nonce` 并明文附加在 Object Header 中。



### 2. 经济学漏洞：P0 阶段遭遇“云账单刺客”（S3 PUT 请求爆炸）

在 **2.1 P0 路线树** 与 **1.1 系统定位** 的结合处，存在一个脱离实际云厂商计费规则的设计断层。

* **缺陷描述：**
规范的定位包含“海量小文件数据集”，但在 P0 阶段的存储布局被设定为 `loose object`（松散对象），而把 `packfile`（打包对象）放到了 P1。
假设用户测试 P0 版本，提交一个包含 10 万个小文件（如 Node.js 的 `node_modules`，总计约 100MB）的代码库。系统会生成 10 万个 File Object 和 10 万个 Chunk Object。
在推送到 S3 兼容后端时，这需要发起 **20 万次 PUT 请求**。
按照 AWS S3 的计费标准（每 1000 次 PUT 收费 $0.005），仅仅为了上传这 100MB 的数据，API 请求费用就高达 **$1.00**（流量费几乎忽略不计）。如果用户一天执行 10 次 Commit & Push，一天光 API 费用就是 $10。不仅如此，大多数廉价 S3 后端的 QPS 限制在 300-500，这 20 万次请求需要一个小时才能传完。P0 在海量小文件场景下，在经济和性能上都处于不可用状态。
* **修复方案：**
* **必须将 Pack Writer 提升到 P0 阶段**（至少是基础的 Pack append-only 写入）。
* 如果为了保持 P0 的工程精简，可以不实现复杂的 Pack Index Compaction 和 Range Read，但上传时**绝对不能**将小体积（< 1MB）的 Chunk 直接作为独立的物理对象上传。必须在本地攒批成 8MB-16MB 的 Pack 物理块后再 PUT。



### 3. 系统级死锁：跨环境（Docker/NFS）下的 PID 锁欺骗

在 **8.5 Local Repo Lock (本地锁)** 的设计中，使用了极其过时的进程级判断机制，这在现代容器化和分布式工作流中会引发数据损坏。

* **缺陷描述：**
规范提到锁文件为 `.e2v/lock`，且判断过期锁的依据是“必须确认进程不存在”。这通常意味着写入 PID，并通过 OS 的 `kill -0 <PID>` 来探活。
问题在于，很多用户会将 `.e2v` 目录挂载到 Docker 容器内运行 CLI，或者放在局域网的 NAS (NFS/SMB) 上供多台电脑使用。
在 Docker 容器内部，CLI 进程的 PID 往往是 `1` 或 `7`。如果容器异常退出，锁文件残留。下一次启动容器时，或者用户在宿主机检查时，宿主机上恰好有一个别的进程 PID 为 1 或 7（宿主机上绝对有活跃的低号进程），系统会误判“写锁依然有效”，导致仓库被永久锁定，只能依赖用户手动删锁（极其危险）。反之，不同机器跨 NFS 挂载时，PID 探活更是毫无意义。
* **修复方案：**
* 废弃基于 PID 探活的 Lock File。
* 针对本地文件系统，必须使用操作系统内核级别的排他锁，如 POSIX 的 `flock()` 或 `fcntl(F_SETLK)`，以及 Windows 的 `LockFileEx`。这种锁的特性是：**一旦进程退出（无论是否崩溃），内核会自动释放文件描述符并解锁**，从根本上杜绝“僵尸锁”。
* 针对跨机器的网络挂载（无法依赖 `flock` 的场景），应该建议用户配置单写者 Lease，或者明确提示不支持跨机器并发写本地目录。



### 4. VFS 读服务崩溃：GC 诱发的“句柄悬空”（ESTALE 错误）

在 **7.6 VFS** 和 **6.14 GC** 的生命周期交集中，ReadService 对物理存储布局的静态绑定会引发挂载点崩溃。

* **缺陷描述：**
规范规定，VFS 句柄（File Handle）绑定在 `layout_generation` 上。这意味着打开文件时，逻辑对象已经解析为对应的物理 Pack 位置。
假设有一个长连接的视频播放器，通过 VFS 打开了一个 10GB 的加密视频，正在缓慢播放。此时，用户在另一个终端执行了 `e2v gc --execute` 或 `e2v repack`，触发了 Pack Compaction。旧的 Pack 被合并成了新的 Pack，并且（在安全期过后）被物理删除了。
视频播放器继续请求后续的 Range Read。此时 VFS 拿着旧的 `layout_generation` 试图去远端拉取旧的 Pack 文件，收到 `404 Not Found`。由于绑定是静态的，VFS 只能向上层应用抛出 I/O 错误（类似 Linux 的 `ESTALE`），导致播放器直接崩溃。
* **修复方案：**
* `ReadService` 的 `read_range` 不能对物理位置做“一锤子买卖”的静态断言。
* 必须引入 **Stale Layout Fallback（失效布局重解析）** 机制。如果在 `read_range` 时遭遇 404，`ReadService` 不能立刻报错，而是应该静默拉取远端最新的 `Layout Root`，用相同的 `logical_object_id` 重新解析出新的物理 Pack 引用，无缝切换过去并完成读取。
* 只有当新的 Layout 中也找不到这个对象时，才真正抛出 I/O 缺失错误。


## reviewer5


### 1. 元数据平面内存爆炸：只有 Tree Sharding，漏了 File Sharding

在 **5.3 Object Model** 和 **5.5 Manifest Store** 中，架构师非常敏锐地察觉到了单级大目录会导致内存溢出，因此引入了 `Tree Sharding`。但是，完全忽略了超大文件（Large Single File）带来的元数据灾难。

* **缺陷描述：**
规范中规定：`max_manifest_plaintext_size: 4 MiB`。
假设我们面临 P1 阶段支持的“大型二进制文件”场景，比如用户提交了一个 200GB 的虚拟机镜像（VMDK）或数据库文件。
按照 `small-files` 或极端碎片化场景，平均/最小 chunk 可能会回退到 64 KiB。
200GB 的文件按 64 KiB 切块，会产生约 **310 万个 Chunk**。
在 `FileObject` 中，每个 Chunk 需要记录 `ObjectId` (32字节 BLAKE3) + `offset` + `length` + 认证标签，至少需要 50 字节。
310 万个 Chunk × 50 字节 = **155 MB**。
这意味着，为了描述这**一个**大文件，系统必须在内存中构建一个 155 MB 的单体 `FileObject`，这不仅直接违背了规范中定死的 4 MiB 上限，还会导致加解密流水线在这一步发生严重的内存抖动（OOM 风险），并在 VFS 读取时造成极其缓慢的解析延迟。
* **修复方案：**
* 必须在 Object Model 中引入 **File Sharding（文件哈希树/间接块机制）**，类似 Linux ext4 的 inode 间接块或 IPFS 的 Merkle DAG。
* 新增对象类型 `file_shard`。当 `FileObject` 内部的 Chunk 列表超过 4 MiB（或包含超过 8192 个 chunk）时，`FileObject` 不再直接保存 Chunk ID，而是保存指向若干个 `file_shard` 对象的指针，由 `file_shard` 真正持有 Chunk 列表。



### 2. 同步与并发：Ref CAS 的“ABA 问题”导致历史静默丢失

在 **6.6 RefStore** 和 **6.11 Sync Engine** 中，关于并发更新分支（Branch/Ref）的设计存在分布式系统中最经典的 ABA 漏洞。

* **缺陷描述：**
规范定义 `compare_and_swap_ref(token, expected, next)`。如果在实现时，`expected` 仅仅被等同于“预期的 Snapshot ID”（这是绝大多数轻量级 Git 实现的做法），灾难就会发生。
假设远端 `main` 分支当前指向 `Snapshot A`。
客户端 1 想要 Push，它拉取到 `expected = A`，本地开始漫长的计算和上传 `Snapshot C`。
在这几十分钟内，客户端 2 强制 Push 了一个修复 `Snapshot B`，随后发现推错了，又通过脚本回滚（强制 Push）回了 `Snapshot A`。此时远端的分支履历实际上是 `A -> B -> A`。
客户端 1 上传完毕，发起 CAS 请求：`如果远端目前是 A，则更新为 C`。后端一看，现在确实是 A，**CAS 成功**。
结果是：客户端 2 曾经做过的 `B` 及其所有可能的衍生物被彻底孤立，发生了静默的历史丢失，且日志里毫无冲突报错。
* **修复方案：**
* Ref CAS 的 `expected` 绝对不能仅仅是业务层面的 Snapshot ID。必须强绑定存储后端提供的**底层世代戳（Generation / ETag / VersionId）**。
* 在 S3 中，必须使用开启版本控制的 VersionId 或特定的 ETag 进行 `If-Match` 条件写；在本地目录或 WebDAV 中，Ref 文件不仅要保存 Snapshot ID，还必须写入一个单调递增的 `ref_version_nonce`。CAS 的对比条件必须是这个底层的 nonce。



### 3. 存储与数据交叉：Packfile 导致的“盲打空间放大”（Blind Packing Amplification）

在 **3.8 Pack Writer** 结合 **CDC 增量去重** 时，如果在分布式环境下同步逻辑处理不当，去重率会断崖式下跌。

* **缺陷描述：**
P1 引入了 Packfile，用来打包小 Chunk。由于系统是多设备的，Pack Index 是分层合并的（immutable segments）。
假设设备 1 上传了一个 10GB 的文件，分散打包在了 100 个 Pack 中。
设备 2 几个月没开机，本地的 Pack Index Cache 非常老旧。此时设备 2 修改了那个 10GB 文件的一行代码，准备 Push。
设备 2 的 Chunker 切出了 99.9% 相同的 Chunk。但在检查“哪些 Chunk 远端已存在”时，由于设备 2 本地的 Pack Index 是旧的，它查不到设备 1 上传的那 100 个 Pack 的引用。
结果：设备 2 认为这 10GB 数据远端全都没有，于是**重新把这 10GB 的 Chunk 打包成全新的 Pack，并全量上传了一遍**。
这在 E2EE 系统中被称为“盲打”（Blind Upload）。因为远端数据是加密的，设备 2 无法向远端发送一堆明文 Hash 询问“你们有这些块吗”（这会破坏零知识/隐私边界）。
* **修复方案：**
* Sync Engine 在每次进入 `build PublishPlan`（打包阶段）之前，**必须强制执行一次轻量级的 `fetch index_root**`。
* 即使不拉取全量数据，也必须把远端所有新增的 Pack Index Segments 拉取到本地并更新 Local Cache。只有在保证本地拥有远端最新 Object 视图的情况下，才能开始 Pack Writer 的工作。否则 CDC 增量机制名存实亡。



### 4. 运行时平面：Tokio 线程池饥饿导致的 VFS 死锁 (Executor Starvation)

在 **8.3 Scheduler**、**7.6 VFS** 和 **5.9 ReadService** 的结合处，存在 Rust 异步编程中最致命的隐患，会导致挂载的盘突然“卡死”。

* **缺陷描述：**
规范提到，外部通过 VFS 读取数据，VFS 调用 `ReadService.read_range`，最终由 `tokio` 调度网络请求。
假设用户用一个多线程扫描工具（如 `grep -r` 或杀毒软件）扫描挂载的 VFS 目录。内核 FUSE 会瞬间向 VFS 发送成百上千个并发的 `read` 请求。
VFS 的 Rust FUSE 库（通常是 blocking 的内核回调）会将这些请求通过 `tokio::spawn` 扔给异步运行时，并阻塞等待结果。
如果 `ReadService` 为了保护本地内存，设置了 `Backpressure`（比如限制同时只能下载 10 个 Pack 块的 Semaphore）。
由于大量的 FUSE blocking worker 都在等待这 10 个下载任务的结果，它们可能会**占满 Tokio 的全部 worker threads**。此时，Tokio 连调度内部 TCP/HTTP 响应解析流（Stream）的线程都没有了。整个 Tokio 运行时陷入死锁：发出去的下载请求在网卡缓冲区里堆积，因为没有空闲线程去 `poll` 它们；而所有线程都在死等这些下载请求返回。
* **修复方案：**
* 绝对禁止 VFS 的 FUSE blocking 回调与处理网络/解密的 Tokio Runtime 共用一个线程池。
* 必须物理隔离：分配一个专属的 `tokio_fs_pool` 专门处理 FUSE/内核传来的阻塞调用，分配另一个独立的 `tokio_net_pool` 专门负责与 `BlobStore` 交互和加解密。
* 使用跨 Runtime 的 `oneshot::channel` 传递数据，确保哪怕 FUSE 队列排了上万个请求卡死，也绝不会饿死处理底层网络 I/O 的核心事件循环。

## reviewer6

### 1. 隐私防线泄漏：本地 FTS 索引造成的“撤销免疫”（Data Remanence）

在 **5.8 Local Index** 与 **P3 历史强撤销** 的结合部，本地搜索为了性能做出的妥协，会直接刺穿端到端加密的隐私边界。

* **缺陷描述：**
规范定义 P1 引入 SQLite FTS5 来提供 `filename FTS` 甚至未来的内容检索。
假设一个场景：用户发现自己不小心把包含敏感密码的配置文件（或商业机密）提交了，于是执行了 P3 级别的“历史强撤销”，远端数据被完全重写、旧 Epoch 密钥被作废，远端确实安全了。
**但是！** 用户本地的 SQLite 数据库（或者同团队其他成员同步过的本地索引）中，依然以**明文**形式保留着那个配置文件的名字、甚至部分元数据和内容。因为 SQLite 的 FTS 表是追加写入的倒排索引，如果不做极度精细的级联清除，即使 Snapshot DAG 删除了，词典树里依然残留着敏感词。
如果这台电脑丢失，攻击者提取 SQLite 缓存，所谓的“强撤销”形同虚设。
* **修复建议：**
* **架构强约束：** 任何涉及 P3 重写或 GC 删除的操作，必须向 `Local Index` 广播 `Purge Event`。
* SQLite 中的索引数据不仅要删除，还必须强制执行 `PRAGMA secure_delete = ON;` 或定期的 `VACUUM`，防止磁盘空闲扇区残留。
* 极高安全模式下（涉及敏感仓库），应允许用户配置**禁用本地持久化 FTS 索引**，仅在内存中进行即时树遍历搜索。



### 2. 操作系统欺骗：文件系统时钟精度导致的“幽灵脏读”（TOCTOU）

在 **3.3 File Scanner** 源文件一致性检查中，依赖 `mtime`（修改时间）作为读前/读后比对的基准，在某些文件系统上注定会引发哈希污染。

* **缺陷描述：**
规范要求“读前/读后 metadata 校验，包括 size、mtime... 发现变化则丢弃重试”。
这在 Linux 的 `ext4` 或 `btrfs`（纳秒级精度）上勉强可行。但在很多外置移动硬盘（FAT32、exFAT，精度为 2 秒），甚至是较早的 macOS HFS+（精度为 1 秒）上，会发生灾难。
假设正在编译一个大工程，编译器以极快的速度写入了一个 `binary.o` 文件。File Scanner 开始读取并切块，耗时 0.5 秒。在 Scanner 读取到一半时，链接器又修改了这个 `binary.o`。
因为这两次修改都发生在**同一个 1 秒/2 秒的时间窗口内**，文件系统的 `mtime` **根本不会更新**！
结果：Scanner 的读前 `mtime` 和读后 `mtime` 完全一致，大小碰巧也一致（例如修改了等长的几个字节）。系统认为读取一致，将一个前半截是旧版本、后半截是新版本的“缝合怪”（Torn Read）打包加密传上了云端。这在代码和数据库备份中是致命的。
* **修复建议：**
* 不能仅依靠 `mtime` 与 `size`。必须在支持的系统上引入 `ctime`（状态改变时间）和底层 API（如 Linux 的 `inotify` 脏标记，或 Windows 的 `USN Journal`）。
* **终极防线：** 如果读前 `mtime` **等于或极度接近当前系统时钟**（比如差值小于该文件系统的精度），必须将该文件标记为 `Volatile`（高危易变）。对其必须施加延迟读取（退避 2 秒后再读），或者强制计算两次全量 Hash 来交叉验证，绝对不能盲目信任 metadata。



### 3. Rust 运行时雪崩：Tokio 阻塞池耗尽（Blocking Pool Exhaustion）

在 **8.3 Scheduler** 和底层的 **3.3/3.7 I/O 流水线** 中，不当使用 Rust 的标准异步 I/O 工具会导致全局死锁。

* **缺陷描述：**
由于操作系统（尤其是 Linux）过去对异步文件 I/O 支持很差，Rust 的 `tokio::fs` 在底层并不是真正的非阻塞 I/O，而是将所有文件读写操作丢进了一个全局的 `spawn_blocking` 线程池（默认最大 512 个线程）。
当用户执行 `e2v commit` 扫描一个包含数万个小文件的仓库（如 npm 项目或庞大的图片库），且存放于较慢的机械硬盘或网络映射盘时。
`File Scanner` 会瞬间发起成百上千个并发的 `tokio::fs::metadata` 或 `tokio::fs::read` 请求。这些请求会瞬间**塞满全部 512 个 blocking 线程**。
此时，整个 E2E 系统的后台网络库试图做 DNS 解析（DNS 解析在 Tokio 中也是阻塞的，需要进同一个池子），或者加密密钥管理试图读取一下本地证书，统统会被卡死，导致网络超时、握手失败、甚至是 UI 完全失去响应。
* **修复建议：**
* **严格隔离 I/O 并发：** 对 `File Scanner` 必须使用 `Semaphore` 将并发 `tokio::fs` 调用严格限制在安全数量（如 64 或 128），绝不能无限 fan-out。
* 对于高性能要求，在 Linux 平台上应彻底绕过 `tokio::fs`，使用原生的 `io_uring` 库（如 `tokio-uring` 或 `glommio`）来实现真正的零阻塞文件系统遍历。



### 4. VFS 兼容性幻觉：mmap 与 SQLite 导致的内核级数据损坏

在 **7.6 VFS** 和 **P3 可写 VFS** 的规划中，把远端对象挂载为本地文件系统时，忽略了 POSIX 标准中最危险的两个功能：`mmap` 和 Byte-Range Lock。

* **缺陷描述：**
一旦 VFS 挂载成功，用户大概率会产生一个错觉：“这是一块普通的硬盘”。
于是，用户在 VFS 挂载的目录里执行 `git clone`，或者用代码直接连上里面的一个 `database.sqlite`。
Git 和 SQLite 都是极端依赖 `mmap`（内存映射文件）和 `fcntl`（区域文件锁）的程序。
如果你的 VFS（无论是 FUSE 还是 WinFSP）只实现了基础的 `read/write` 接口，而没有实现复杂的 Page Cache 同步机制和分布式锁机制，当 SQLite 试图通过 `mmap` 修改并刷新脏页时，由于 VFS 底层的 Chunk 是加密不可变的，写回操作将发生完全错位。更可怕的是，如果不拦截锁请求，SQLite 会以为自己拿到了排他锁，从而与其他进程发生并发写入。
结果：数据库瞬间损毁（Corruption），且是在加密层被悄悄写坏的，下次挂载连解密都会失败。
* **修复建议：**
* **尽早打消全功能 VFS 的幻想。** 必须在架构文档的 `Backend Capability` 层面明确宣告：本 VFS 仅提供流式访问（Stream-only Access）。
* 在 FUSE/WinFSP 的接口实现中，**必须显式拦截并拒绝**包含 `MAP_SHARED` 的 `mmap` 系统调用（返回 `ENODEV`），以及显式拒绝非法的范围锁。让尝试使用它们的上层应用（如 SQLite）明确收到错误并安全降级或退出，而不是让它们在错觉中把数据写烂。


## reviewer7


### 1. 内存物理极限：GC “可达性集合”导致的亿级节点 OOM

在 **6.14 GC / Verify** 环节，设计文档轻描淡写地写了一句：“walk reachable snapshot/tree/file/chunk -> **build reachable set** -> find unreachable”。

* **缺陷描述：**
这是所有去重备份系统（如早期的 Restic）都踩过的血坑。
假设目标是一个包含 1000 万个小文件的代码库（经历了 100 次历史快照），或者被切成了 5000 万个 Chunk 的数据集。
在执行 GC 时，为了计算“哪些对象不需要被删”，系统需要在内存中构建一个 `HashSet<ObjectId>`。
每个 `ObjectId` 占用 32 字节。5000 万个节点，加上 Rust `HashSet` 底层的哈希桶开销、指针和装载因子，这个“可达性集合”在内存中会轻易膨胀到 **4 GB 到 8 GB**。
如果用户是在一台只有 8GB 内存的轻薄本、或者只有 1GB 内存的软路由/NAS 上执行后台后台 GC，操作系统会直接触发 OOM-Killer（内存耗尽杀手），把 `e2v` 进程无情杀死。用户将**永远无法在这个设备上成功执行 GC**。
* **修复建议：**
* 架构规范中必须明文禁止在内存中构建全量 DAG 的 Reachable Set。
* **引入外部排序与布隆过滤器（External-Memory Graph Traversal）：** GC 的 Mark 阶段必须将扫到的可达 ID 写入本地 SQLite 的临时表或 RocksDB 中，利用磁盘进行排序和去重。最后通过数据库的 `LEFT JOIN` 或者流式比对（Two-pointer stream merge）来找出未引用的对象，将内存峰值严格控制在 100MB 以内。



### 2. 安全漏洞：Manifest 投毒导致的端到端 “Zip-Slip” 路径穿越

在 **3.3 File Scanner** 和 **5.9 ReadService / Checkout** 的设计中，存在极大的“盲目信任”危机。

* **缺陷描述：**
规范提到在 Scan 时会执行 `portable-strict`（禁用 `../`、绝对路径等）。但这只是**写入端**的君子协定。
这是一个多设备的同步系统，甚至未来有 SDK。假设攻击者（或被植入木马的协作者设备）绕过 CLI，直接调用 LogicalObjectStore 构造了一个恶意的 Tree 对象，里面包含一个条目：`name = "../../../../../etc/passwd"` 或 `name = "../../../.ssh/authorized_keys"`，并合法加密推送到远端。
当你的高管或同事在本地执行 `e2v checkout` 或后台开启 Sync 时，本地客户端解密了这个 Tree，看到这个路径，直接拼接到了目标工作区目录上。
结果：**操作系统的关键系统文件被静默覆盖，引发 RCE（远程代码执行）或彻底控制主机。**
* **修复建议：**
* **零信任 Checkout：** Checkout 和 ReadService 绝对不能信任从远端解密出来的 Manifest。解密后的路径，必须再次（甚至比 Scan 时更严格地）经过一层 **Path Jail（路径沙箱）** 的拦截验证。
* 任何试图逃逸当前 Checkout Root 的路径（如包含 `..`、绝对路径斜杠 `/` 开头、NTFS 的 `C:\` 或 ADS 备用数据流 `:`）必须在写入磁盘前触发致命错误并中断整个操作。



### 3. 密码学解析漏洞：未经认证的对象 Header 导致拒绝服务或降级攻击

在 **3.6 Encryptor** 和 **8.9 Format Boundary** 的解密流程中，加密对象的数据结构存在“先解析后认证”的悖论。

* **缺陷描述：**
规范定义加密封包包含：`[magic, format_version, object_type, crypto_suite, padding_policy, object_id, nonce, ciphertext, auth_tag]`。
在执行 AEAD 验证之前，解析器必须先读取 `crypto_suite` 才知道用什么算法，读取 `padding_policy` 和 `format_version` 才知道怎么截取密文。
**致命漏洞：** 这些前置字段（Header）在被读取时，**尚未经过 AEAD 的完整性校验**。
如果中间人或恶意存储后端篡改了 `format_version` 为一个超大数字，或者把 `crypto_suite` 改为一个未来可能废弃的弱算法标识，甚至故意构造畸形的 `padding_policy` 长度字段。
Rust 的解析器在试图分配内存或匹配 Enum 时，会直接发生 Panic、越界读取或内存分配失败（OOM），导致系统在执行任何密码学验证之前就直接崩溃（DoS 攻击）。
* **修复建议：**
* **严控未认证读取：** 在读取头部解析路由时，对于变长字段绝对禁止直接使用数据包内的长度进行内存分配。
* 更专业的做法是：对象的头部信息（Header）必须作为 **AEAD 的 AD (Associated Data)**。虽然必须先读它才能解密，但在解密流程完成并返回 `Ok` 之前，上层业务逻辑**绝对不能**信任和采纳这些 Header 中的任何状态信息。若 AD 验证失败，说明 Header 被篡改，直接丢弃整个对象。



### 4. 跨语言崩溃：C-ABI (P2) 的跨 FFI 边界 Panic 栈展开 (Unwind) 灾难

在 **7.7 SDK / C-ABI** 的底层工程规划中，漏掉了 Rust 导出到 C 时最可怕的未定义行为（Undefined Behavior, UB）。

* **缺陷描述：**
规范要求提供 “opaque C handles” 并且“所有错误通过错误码和错误字符串句柄返回”。
但是，Rust 内部的代码（特别是底层的 `tokio` 运行时、加解密库、文件 I/O）在遇到极端情况时（如内存分配失败、越界访问、断言失败、甚至某些特定库内部的 `unwrap` 被触发时）会发生 **Panic**。
如果这个 Panic 发生在 C-ABI 的导出函数中，并且没有被捕获，Rust 的栈展开（Stack Unwinding）机制会直接冲破 FFI 边界，穿透到调用方（比如 C++、Python、C#、Go 的宿主进程）的堆栈中。
结果是：宿主进程遭遇 UB，大概率发生 Segment Fault（段错误）并瞬间闪退。这对于想要接入你 SDK 的桌面 UI 应用或服务器来说是绝对不可接受的。
* **修复建议：**
* 所有的 C-ABI 导出函数（`#[no_mangle] pub extern "C" fn`）的内部，最外层必须，且毫无妥协地包裹在 `std::panic::catch_unwind` 中。
* 如果捕获到了 Panic，必须在 `catch_unwind` 的 `Err` 分支中，将其转换为一个通用的 `E_INTERNAL_PANIC` 错误码，安全地返回给 C 端，确保宿主进程永远不会被 SDK 连累崩溃。

## reviewer8


### 1. 密码学劫持：Merkle 树的“类型混淆”碰撞攻击 (Type Confusion)

在 **3.5 Hasher** 和 **5.3 Object Model** 的设计中，存在一个经典的 Merkle DAG 致命密码学漏洞。

* **缺陷描述：**
规范定义 `object_id = BLAKE3_keyed(repo_object_id_key, canonical_plaintext)`。
假设我们有两个不同的对象类型：一个是 `Chunk`（纯粹的文件数据），一个是 `Tree`（包含目录结构的元数据）。
如果攻击者知道目标仓库的目录结构（或者通过猜测），他可以自己在本地构造一个**完全合法的 `Tree` 对象的序列化字节流**。然后，他把这段字节流当作一个普通的文本文件（Chunk）提交到仓库中。
由于这两个对象在底层的字节完全一样，BLAKE3 算出的 `object_id` 也绝对一样。
**灾难发生：** 如果这个假冒的 Chunk 先被上传，远端已经存在了这个 `object_id`。当系统未来真正生成那个 `Tree` 时，由于去重机制，系统会跳过上传。此时，如果逻辑层没有严格校验下载下来的对象到底是什么类型，攻击者就可以通过上传一个“文件”，暗中替换或劫持整个“目录树”的解析，引发反序列化灾难或目录结构错乱。
* **修复方案：**
* **Domain Separation（域分离）：** Hash 计算绝对不能只针对 `canonical_plaintext`。
* 计算 `object_id` 时，**必须强制将对象类型和长度作为前缀混入 Hash**。公式必须严格修改为：
`object_id = BLAKE3_keyed(repo_key, ObjectType_Byte || Length_u64 || canonical_plaintext)`。
* 这样，哪怕 Chunk 和 Tree 的底层明文完全一样，它们的 ID 也截然不同，从数学上杜绝类型混淆攻击。



### 2. 分布式理论陷阱：“能力探测”无法证明强一致性 (The CAP Theorem Lie)

在 **6.9 Backend Capability** 和 **6.10 Backend Adapter 准入测试** 中，规范试图用工程手段解决一个数学上不可解的问题。

* **缺陷描述：**
规范提到，通过“适配器准入测试”来探测后端是否具备 `has_strong_consistency`，并以此决定是否允许执行危险的 `gc --execute`。
**致命漏洞：强一致性是无法通过短时间的黑盒测试“探测”出来的。**
假设你挂载了一个基于 Ceph 或多节点 MinIO 的 WebDAV 存储。在网络畅通的周二上午，你的准入测试写入一个文件，立刻 `LIST`，文件存在，测试完美通过，系统将 `has_strong_consistency` 设为 `true`。
到了周五晚高峰，集群由于负载过高发生了暂时的复制延迟（Replication Lag）。此时触发了 GC 流程，执行 `LIST`。由于最终一致性的延迟，刚上传的对象没有出现在列表中，GC 认为它们是垃圾，**直接物理删除**。
你以为你测出了强一致性，其实你只是在系统“状态好”的时候碰巧没撞见它的弱一致性。
* **修复方案：**
* 放弃通过代码“动态测试”强一致性的幻想。
* **必须采用硬编码白名单：** 只有在代码中明确定义为提供强读写一致性的后端（例如明确的本地磁盘、明确声明强一致的 AWS S3 标准版），`has_strong_consistency` 才能为 `true`。
* 对于任何未知的 S3 兼容版、WebDAV 或 Alist，必须**默认且强制为 `false**`。绝不允许通过简单的 ping-pong 测试就解锁危险的自动 GC。



### 3. Rust 底层死穴：常驻 VFS 守护进程的内存碎片化雪崩 (Allocator Fragmentation)

在 **3.4 Chunker**、**3.7 Stream Pipeline** 和 **7.6 VFS** 的结合中，隐藏着一个 Rust 系统级编程极易忽略的 OOM 陷阱。

* **缺陷描述：**
规范定义默认 Chunk 大小为 1 MiB 到 8 MiB。
当用户通过 VFS 挂载仓库，并在其中播放视频、执行大型编译或使用 `rsync` 拷贝大量数据时，`e2v` 的后台常驻进程会在堆（Heap）上疯狂分配和释放 1 MB 到 8 MB 的 `Vec<u8>` 缓冲区（用于网络接收、解密和 FUSE 传输）。
在 Linux 上，Rust 默认使用系统自带的 `glibc malloc`。`glibc` 在处理这种高频的、多线程交错的大块内存分配与释放时，会产生**极其严重的内存碎片（Memory Fragmentation）**。
你的代码可能没有任何内存泄漏（Leak），所有的 `Vec` 都被正确 `drop` 了，但是操作系统的 RSS（驻留集大小）会持续飙升，几小时后从 50MB 涨到 2GB，最终被内核的 OOM-Killer 物理超度。VFS 毫无征兆地崩溃断开。
* **修复方案：**
* **更换全局分配器：** 对于这种数据密集型后台守护进程，必须在 `main.rs` 中显式抛弃系统默认分配器，引入 `jemalloc` 或 `mimalloc`。
* **引入内存池（Memory Pool）：** 在 Stream Pipeline 中，绝对禁止每次遇到新 Chunk 都 `vec![0; len]`。必须使用 `bytes::BytesMut` 或者自定义的 Object Pool，循环复用那些 1MB - 8MB 的大块内存。



### 4. 协议级攻防：恶意后端的“状态回放攻击” (Rollback/Replay Attack)

在 **6.6 RefStore** 和 **4.5 Keyring** 中，系统在假设存储后端是“诚实但好奇”（Honest-but-Curious）的，但忽略了后端可能是“完全恶意”的。

* **缺陷描述：**
E2EE 保证了后端看不懂你的数据，CAS 保证了并发不会写坏数据。但是，如果你使用的是一个不受信任的 WebDAV（比如某个被黑客控制的免费网盘）。
你今天 Push 了一个最新的快照（包含你最新的密码、日记、以及吊销旧设备的 Keyring）。
黑客无法解密你的数据，但他可以**把整个云盘的数据，直接通过快照回滚到半年前的状态**。
明天你打开电脑，客户端连上服务器，拉取了 `Ref` 和 `Keyring`。
客户端对所有的密文和签名进行校验——发现**全部合法**！（因为这确实是你半年前亲自加密并签名的合法数据）。
系统静默接受了这个状态。结果是：你这半年的数据凭空蒸发了，并且你之前吊销的旧设备（由于 Keyring 也被回滚了）重新获得了访问权限！
* **修复方案：**
* 在分布式 E2EE 系统中，防范回放攻击不能仅靠远端状态。
* **引入本地高水位标记（High-Water Mark）：** 客户端必须在操作系统本地（比如 `~/.config/e2v/`，绝对不能放在被同步的仓库目录内）记录一个单调递增的 `highest_seen_ref_generation` 和 `highest_seen_keyring_generation`。
* 如果客户端拉取远端状态时，发现远端的 Generation **低于**本地记录的高水位，必须触发致命级别的红色警报（CRITICAL_ROLLBACK_DETECTED），立刻冻结所有读写操作，并通知用户存储后端可能遭到篡改或发生严重故障。


## reviewer9


### 1. Rust 异步陷阱：Tokio 取消安全（Cancellation Safety）导致的数据静默损坏

在 **3.7 Stream Pipeline** 和 **6.8 TransactionPublisher** 中，大量使用了 `tokio` 异步编程来处理高并发读写。

* **缺陷描述：**
在 Rust 中，异步任务（Future）是可以被随时“取消”的（例如网络超时、用户按 `Ctrl+C`、或者 `tokio::select!` 竞争失败）。
当一个 `await` 点被突然取消时，函数的执行会**立刻中断**，不会执行后面的清理代码。
假设系统正在执行 Checkout，把加密的 Chunk 拼接解密并写入本地的 `sqlite` 状态表和本地临时文件。此时遇到了一个 10 秒的网络超时断开，`tokio` 取消了这个任务。
如果你的 `TransactionPublisher` 或者 `Checkout Pipeline` 没有做到严格的 **Cancellation Safe（取消安全）**，那个写了一半的临时文件会被遗留，SQLite 的本地事务由于没有正确触发 `rollback`（如果没用 `Drop` guard 保护的话）可能处于挂起或半脏状态。
下次重试时，逻辑层以为这个文件是干净的，或者从一半开始续写，直接导致本地检出的文件产生二进制错位（静默损坏）。
* **修复建议：**
* 任何涉及磁盘状态变更、数据库事务、共享锁的状态机，**绝对不能依赖顺序执行的代码来做清理**。
* 必须强制使用 Rust 的 `Drop` Trait（类似于 RAII 模式）来封装所有的副作用动作。即使 Future 被无情腰斩，`Drop` 依然会被编译器确保调用，从而安全地回滚事务、删除半残的临时文件、释放本地锁。



### 2. 操作系统硬防线：并发 Checkout 引发 `EMFILE` (Too Many Open Files) 进程崩溃

在 **5.9 ReadService** 和 **3.3 File Scanner** 中，架构设计了极其优秀的背压（Backpressure）来控制内存，但漏掉了一个 OS 级别的核心资源限制。

* **缺陷描述：**
当用户拉取（Clone / Checkout）一个包含 10 万个小文件的代码库（比如前端包含大量 `node_modules`）时，`tokio` 会非常高效地调度成千上万的轻量级任务去并发执行 `File::create()` 和 `File::write_all()`。
**灾难爆发：** 绝大多数操作系统（macOS 默认限制 256 或 1024，Linux 通常是 1024）对单个进程同时打开的文件描述符（File Descriptor, FD）有严格上限。
当你的并发写入任务瞬间超过 1024 个时，操作系统的 `open()` 系统调用会直接返回 `EMFILE` 或 `ENFILE` 错误。此时，不仅后续的 Checkout 会全部报错，整个系统的网络 Socket 也会因为分配不到 FD 而断开连接，引发全线崩溃。
* **修复建议：**
* 仅仅做内存背压是不够的，必须在全局引入 **File Descriptor 专用的并发信号量（FD Semaphore）**。
* 整个 App 启动时，必须动态查询当前 OS 的 `ulimit -n`，提取出一个安全值（例如上限的 80%），初始化该信号量。
* 任何 `File::open`、`File::create` 和 `TcpStream::connect` 操作，必须先 `acquire()` 这个信号量，用完后释放，从根本上杜绝操作系统资源耗尽引发的雪崩。



### 3. 隐私架构刺客：半公开 Pack Index 导致 Padding Policy（填充策略）破功

在 **3.8 Pack Writer** 的规划和 **3.6 Encryptor** 中，这里藏着一个自相矛盾的隐私泄露点。

* **缺陷描述：**
为了隐藏文件大小，防止黑客推断数据内容，规范在 3.6 中极其专业地引入了 `PaddingPolicy`（加密填充）。
但是！在 3.8 的说明里提到，Pack Index（记录每个 Chunk 在大 Pack 文件里的 offset 和 length 的索引）可能会是“是或半公开”**的状态（有时候为了 CDN 加速或边缘节点解析，不加密 Index Root）。
**逻辑死穴：** 如果 Pack Index 没有被最高级别的密钥加密，任何能够访问存储桶的人，只要读取了 `pack_index`，看到了密文 Pack 内部每个 Chunk 的 `length`，他就能**精确知道每个原始数据块加密后的大小！
这等于把你辛辛苦苦在 3.6 做的 `PaddingPolicy` 和流量混淆（Traffic Shaping）直接底牌看穿。攻击者再次获得了“精准文件大小特征”，CDC 加密去重带来的隐私威胁卷土重来。
* **修复建议：**
* 必须在架构级别下达死命令：**无论是 Pack 本身，还是 Pack Index，必须 100% 被 AEAD 强加密！绝不允许任何“半公开”的索引层。**
* `pack_index` 只能包含密文的 offset 和 length，并且它自己也必须作为一个常规的 Encrypted Object 存储。不掌握密钥的 CDN 只能把它当盲块缓存，绝对不能让存储后端看懂 Index。



### 4. 分布式谎言：虚假的“S3-Compatible”导致的 CAS 静默失效

在 **6.10 Backend Adapter** 与 **6.6 RefStore** 中，整个系统的多端安全命脉建立在远端的 CAS（Compare-And-Swap）能力上。

* **缺陷描述：**
规范中说：“multi-writer push 必须依赖后端 CAS/conditional put”。对于 S3 兼容后端，通常使用 `If-Match: <ETag>` 来实现原子更新。
**地狱级坑点：** 市面上有大量的所谓“S3 兼容”存储（早期版本的 MinIO、部分小云厂商的 OSS/COS、以及各种自建的 Ceph RGW），它们的 S3 API 是**残缺的**。
当客户端发送带有 `If-Match` 头的 PUT 请求时，某些劣质后端不仅**不支持**条件判断，更致命的是，它们**不会报错，而是直接忽略这个 Header 并强制覆盖写入！**
结果：你的客户端认为自己执行了一次安全的 CAS 提交，但实际上它执行了一次野蛮的盲写。多端并发 Push 时，分支引用会被悄无声息地相互覆盖，历史记录彻底丢失，且没有一行 Error 日志。
* **修复建议：**
* 后端适配器（Backend Adapter）的准入测试（Capability Detection）必须包含“预期失败测试”（Negative Testing）。
* 在初始化一个 Remote 时，客户端必须在远端尝试执行一次必定失败的 CAS 请求（故意传入一个错误的 `expected_version`）。
* **如果该请求返回了 `200 OK`（写入成功），说明这个后端在撒谎！** 客户端必须立刻熔断该后端的 `supports_conditional_put` 标识，强制降级到无 CAS 模式（依赖单写者租约，或者直接拒绝多端 Push）。

















