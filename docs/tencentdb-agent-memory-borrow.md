# TencentDB Agent Memory 可借鉴点调研

> 调研日期：2026-08-13<br>
> 对方仓库：[`TencentCloud/TencentDB-Agent-Memory`](https://github.com/TencentCloud/TencentDB-Agent-Memory)<br>
> 对方默认分支：`feat/server_team`<br>
> 固定审阅版本：[`4dca55c41bf11cb19b49728dbe495c8e05d25abb`](https://github.com/TencentCloud/TencentDB-Agent-Memory/commit/4dca55c41bf11cb19b49728dbe495c8e05d25abb)<br>
> 本地 `mem` 对照版本：工作区 `bf5256eb4a33a052c3171d21d1f598155ffe257a`；这是源码审阅快照，不代表已部署版本。806 条胶囊等运行数据来自任务背景，不作为源码事实。

## 1. 结论先行

TencentDB Agent Memory 最值得借鉴的并不是它的向量检索，而是它把“记忆”提升成了可治理、可装配的 Agent 资产：用户、团队、Agent、任务、资产、固定绑定和 ACL 都有一等模型；同时把上下文拆成稳定画像、动态原子记忆和按需知识工具三条注入通道。它的 L0-L3 分层和场景导航也对降低每轮上下文体积有直接价值。

本地 `mem` 在检索排序、生命周期、反馈调权、可审计版本链、演化治理和实体知识图谱上更成熟，不宜用对方的 L0-L3 或 LLM 自动合并流程整体替换。最合理的路线是保留现有胶囊事实层和检索管线，先增加轻量的 **Agent Loadout（按 Agent 装配记忆）**，再把场景导航和分层注入做成派生视图。

推荐优先级：

1. **可信 Agent identity + 授权边界 + Loadout-lite**：最高优先，补齐多 Agent 治理的真实缺口。
2. **稳定 / 动态 / 工具引用三通道上下文契约**：小步、低风险，能立刻约束 prompt 注入。
3. **跨 Agent 工具轨迹 → 审查型 Skill 提案**：复用 transcript/结晶能力，自动找候选，但不让 LLM 直接改活跃资产。
4. **场景导航派生视图**：用现有胶囊、episode 和 KG 生成 L2-like 导航，不改写事实。
5. **版本化 Skill Bundle + 会话版本固定**：让结晶后的 Workflow 携带脚本/模板，同一会话内不被 head 漂移影响。

## 2. 对方项目概览

### 2.1 定位：从个人记忆到团队级 Memory Hub

项目把自身定位为面向 Agent 的“团队级记忆中枢”，产品面覆盖 Chat Memory、Skill、Wiki 和 CodeGraph；README 明确强调目标不是只保存聊天，而是把经验、能力和知识变成可复用资产。[证据：README_CN 定位与四类资产](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/README_CN.md#L54-L72)

它与常规 RAG 的区别也写得很清楚：RAG 主要回答“能否检索到”，该项目额外提供 Owner、版本、状态、团队共享、Agent Loadout 和 ACL，回答“谁拥有、谁能用、以什么版本装配给哪个 Agent”。[证据：与 RAG 的对照](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/README_CN.md#L169-L193)

因此，它不是单纯的 TencentDB 向量检索样例。向量库只是 L0/L1 检索的一种后端；项目的差异化主要在记忆分层、资产治理、固定绑定和多种注入方式。

### 2.2 模块架构

| 模块 | 实际职责 | 证据 |
|---|---|---|
| `MemoryCore` | L0-L3 记忆、Skill 与资产元数据的核心服务；提供独立 HTTP Gateway，不运行 Agent；知识正文交给 `MemoryKnowledge` | [MemoryCore 边界](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/README_CN.md#L1-L20) |
| `MemoryProxy` | 协议中立的 LLM 代理与注入流水线，通过有序 slot 和缓存策略插入画像、场景导航和工具说明 | [pipeline 接口与执行顺序](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryProxy/src/injection/pipeline.ts#L1-L24), [注入与缓存](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryProxy/src/injection/pipeline.ts#L130-L175) |
| `MemoryPanel` | 团队、Agent、资产、绑定和权限管理 UI/API | [Hono/TypeScript 包定义](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryPanel/package.json#L1-L34) |
| `MemoryKnowledge` | Wiki 构建、FTS5/图扩展检索，以及基于第三方 CodeGraph 的代码符号/调用图工具 | [Knowledge 两类引擎](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryKnowledge/README.md#L8-L18), [依赖栈](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryKnowledge/package.json#L25-L53) |

### 2.3 技术栈和部署形态

- 主体是 Node.js 22+ / TypeScript，HTTP 层以 Hono 为主；`MemoryCore` 依赖 AI SDK、jieba、OpenTelemetry、tiktoken、`sqlite-vec`，并可选 MongoDB、Redis 等。[证据：本地模式要求](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/README_CN.md#L40-L54)，[Core 依赖](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/package.json#L100-L140)
- 仓库许可证文件明确采用 MIT。[证据：LICENSE](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/LICENSE#L1-L27)
- 独立模式使用 SQLite、`sqlite-vec`/FTS5、本地文件和进程内状态；服务模式部署文档列出 Tencent Cloud VectorDB、COS 和 Redis。[证据：两种模式](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/README.deployment.md#L6-L20)，[服务依赖表](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/README.deployment.md#L44-L151)
- L0/L1 记忆检索后端实际支持 SQLite 或 Tencent Cloud VectorDB；没有发现 PostgreSQL 记忆后端。[证据：store factory](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/store/factory.ts#L45-L134)
- 管理元数据后端是 SQLite 或 MongoDB；MySQL 分支明确仍是占位实现。MongoDB 并不是 L0/L1 向量记忆库。[证据：metadata factory](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/metadata/store/factory.ts#L59-L169)

需注意“开源可复现边界”：TCVDB 和记忆元数据 MongoDB 适配器在仓库中，但 Redis state 与 COS storage 通过动态导入 `src/integrations`；包发布配置还明确排除了该目录，而本次固定 SHA 的仓库中未找到对应实现。因此完整云服务拓扑不能仅靠当前仓库开箱复现。[证据：Redis 私有子模块降级说明](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/state/index.ts#L49-L63)，[COS 动态导入与报错](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/storage/factory.ts#L24-L45)，[发布排除 integrations](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/package.json#L49-L77)

## 3. 关键实现拆解

### 3.1 记忆分层与存储 schema

项目的 L0-L3 是**同一会话经验逐步提炼的物化层级**，不是四种并列的语义类型：

- **L0 Raw Conversation**：原始消息；本地文件模式按日 JSONL，一行一条消息，字段包括 message id、role、content、timestamp、session/task/team/user/agent 等；服务模式则可走 StorageAdapter/COS，因此不应把“每日 JSONL”当成所有部署的固定形态。[证据：L0 记录器设计](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/conversation/l0-recorder.ts#L1-L15)，[L0 类型](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/conversation/l0-recorder.ts#L28-L58)，[SQLite L0 表/向量表](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/store/sqlite.ts#L726-L795)
- **L1 Atomized Memory**：从对话抽取的原子记忆，类型包括 persona、episodic、instruction、work fact/task/method/artifact；包含 priority、scene、来源 message ids、version 和完整 scope 字段。本地路径下 JSONL 可作 append-only 恢复源、SQLite 作查询引擎；服务路径可以换成 StorageAdapter 和 TCVDB。[证据：L1 类型和存储角色](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/record/l1-writer.ts#L1-L17)，[L1 record schema](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/record/l1-writer.ts#L30-L98)，[SQLite L1 表/向量表](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/store/sqlite.ts#L604-L724)
- **L2 Scene**：按场景写入 Markdown，头部含 created/updated/summary/heat；导航只暴露 path、summary 等索引信息，并按 heat 排序，正文按需加载。[证据：场景文件格式](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/scene/scene-format.ts#L1-L68)，[渐进式场景导航](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/scene/scene-navigation.ts#L1-L21)
- **L3 Persona**：长期画像，和 L2 一样按 team+agent 定位，刻意跨 session、user 和 task 聚合；L0/L1 则保留细粒度 scope。[证据：L2/L3 scope 语义](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/profile/profile-sync.ts#L20-L27)，[profile record 与能力声明](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/store/types.ts#L240-L293)

README 对这一分层的运行方式概括为：优先使用 L2/L3，必要时通过 BM25、向量和 RRF 回查 L1/L0；Wiki/CodeGraph 则先注入工具说明，正文只有实际工具调用时才进入上下文。[证据：L0-L3 与渐进式披露](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/README_CN.md#L216-L246)

“分片”方面，源码证实的维度是 team/agent/user/session/task scope、L0/L1 按日 JSONL 文件，以及 L2/L3 team+agent profile；未发现一致性哈希、物理数据库 shard 或跨节点分片算法。不能把业务 scope 直接称为数据库分片。

### 3.2 提取、更新、去重、遗忘与压缩

1. **提取调度**：pipeline manager 负责 L0 capture/buffer，并为 L1/L2/L3 使用串行队列；L1 按消息阈值/空闲触发，L2 定时提炼场景，L3 生成画像，并使用逐步增大的 warm-up 阈值。[证据：四层流水线](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/utils/pipeline-manager.ts#L1-L74)
2. **L1 质量门和去重**：先做质量判断、场景切分和抽取；写入前先向量搜候选，失败时回退 FTS，再让 LLM 对一个候选批次选择 `store / skip / update / merge`，支持多目标及跨类型合并。[证据：抽取与质量门](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/record/l1-extractor.ts#L145-L183)，[候选检索](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/record/l1-dedup.ts#L32-L130)，[四种裁决 prompt](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/prompts/l1-dedup.ts#L16-L42)
3. **L1 更新语义**：`update/merge` 会从查询存储删除旧目标后写新 record；JSONL 中旧记录仍留作备份/恢复，但没有发现等价于本地 `supersedes_capability_capsule_id` 的在线可审计版本链。因此这是“生成式改写 + 替换”，不应与本地版本链混同。[证据：writer 的更新/合并职责](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/record/l1-writer.ts#L145-L242)
4. **场景压缩**：L2 extractor 让 LLM 在受限目录中创建、更新和合并场景文件，达到场景上限后强制合并。失败时的本地备份/恢复只适用于 local-fs 路径，StorageAdapter/COS 路径不应被概括为有同等回滚。[证据：受限文件编辑和 local-fs 回滚](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/scene/scene-extractor.ts#L241-L405)
5. **遗忘**：cleaner 可按年龄直接清理 L0/L1，并设置最小保留条数；配置默认是 `0`，即关闭。没有发现基于实际召回反馈的衰减/强化算法。[证据：年龄清理](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/utils/memory-cleaner.ts#L69-L180)，[默认关闭与最小天数](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/config.ts#L364-L377)

另有 `offload` 功能处理 LLM 消息上下文/工具结果的卸载，它不等于记忆生命周期中的压缩或遗忘，本报告不把两者混为一谈。

### 3.3 检索与排序

- store 接口声明 vector、FTS、native hybrid 和 sparse 等能力，具体路径根据后端选择。[证据：检索能力接口](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/store/types.ts#L240-L257)
- TCVDB 路径使用服务端 dense + BM25 sparse，再以 RRF `k=60` 融合；不支持时退化到 dense-only，异常返回空结果。[证据：TCVDB hybrid 与 RRF](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/store/tcvdb.ts#L834-L940)
- SQLite 路径并行执行 FTS 和 embedding search，再在客户端以 RRF `k=60` 融合。[证据：SQLite 双路并行与 RRF](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/hooks/auto-recall.ts#L624-L773)
- 本次审阅未发现 L1 最终排序把 `priority`、confidence、freshness、decay、使用反馈或图信号纳入分值；`priority` 会随结果返回，但不是上述 RRF 的计算项。管理资产表的 `confidence/usage_count/expires_at` 也不能据此推断成 L1 recall ranking 信号。

这意味着对方在“团队资产治理与分层注入”上强，而本地 `mem` 在检索排序上更完整：当前 [`retrieve.rs`](../src/pipeline/retrieve.rs) 将 Tantivy BM25、Lance ANN 和 KG 作为候选通道，并把 scope、类型、confidence、validation、freshness、decay、graph 与 RRF 一并计分；这部分没有理由迁移到对方方案。

### 3.4 Prompt 注入：三种上下文，不是一条召回列表

对方最值得借鉴的检索后设计，是明确区分三种上下文：

- **稳定上下文**：L3 persona、L2 场景导航、工具说明，追加到 system context；
- **动态上下文**：本轮 query 命中的 L1，放在 user message 前，并设单条和总字符预算；
- **按需内容**：Wiki、CodeGraph、Skill 等只注入工具/资源清单，正文由 Agent 调工具后才进入上下文。

Core 自动召回 hook 的对应实现可见 [稳定与动态内容的组装](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/hooks/auto-recall.ts#L150-L312) 和 [字符预算](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/hooks/auto-recall.ts#L835-L899)。OpenClaw adapter 也把 L1 与 L2/L3 明确分区格式化。[证据：adapter formatter](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/openclaw-plugin/src/format.ts#L40-L108)

但注入策略并非全项目统一。`MemoryProxy` 的 TDAI injector 在 session init 注入 L3 全文（截断 6000 字符）和 L2 索引，不自动召回 L0/L1，而是在历史/个性化问题时指导模型调用工具，且总搜索次数最多 3 次。[证据：Proxy 画像/场景注入](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryProxy/src/injection/injectors/tdai-profile-memory-injector.ts#L57-L143)，[L0/L1 按需工具策略](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryProxy/src/injection/injectors/tdai-profile-memory-injector.ts#L163-L210)

因此可借鉴的是“三通道上下文契约”和 progressive disclosure，而不是照抄某一个 adapter 的 prompt 文本。

### 3.5 团队资产、Loadout 与 ACL

对方 schema 有一等的 users、teams、agents、tasks、assets；asset 包含 owner、source、version、visibility、status、confidence、expiry、usage 和 content reference。[证据：主体表](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/scripts/db/sqlite-init.sql#L13-L168)

Agent Loadout 由固定绑定表达：`agent_id + asset_id`，并保存 `injection_mode` 和 `priority`；ACL 的 subject 可以是 user、team role 或 agent，permission 独立建模。[证据：binding 与 ACL schema](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/scripts/db/sqlite-init.sql#L171-L199)

权限检查不是 UI 装饰：源码执行 owner、团队成员、private/restricted、ACL 和 team role 判定，并限制谁可以建立绑定。[证据：权限规则](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/metadata/service/permission-checker.ts#L43-L171)

本地 `mem` 的 [`capability_capsule.rs`](../src/domain/capability_capsule.rs#L9) 有 tenant、Global/Project/Repo/Workspace scope、Private/Shared/System visibility 和 `source_agent`；但除 Diary 专用接口外，`source_agent` 不是通用访问控制。通用搜索请求没有主体权限或 visibility 过滤字段，当前搜索路径也没有按 visibility 授权。因此本地既没有 user/team/agent 主体注册、ACL，也没有“某个 Agent 固定装配哪些资产、以何种注入模式和优先级”的一等模型。[本地搜索请求](../src/domain/query.rs#L11)，[本地搜索入口](../src/service/capability_capsule_service.rs#L1650)

同时要避免高估对方：固定绑定的 schema、管理 API 和 bind 可见性检查是完整代码，`MemoryProxy` 也确实读取 `chat_memory` 绑定，让当前 Agent 加载自身与至多两个借入 Agent 的 L2/L3；但 resolver 只消费 `chat_memory` 的 asset id，并未按返回的 `injection_mode` 分流，旧 L1 recall injector 还明确不再注册。换言之，对方已经有“资产注册 + 绑定 + 部分运行时消费”，但并非四类资产都统一执行 `direct/summary/tool/reference` 的通用 Loadout 引擎。[固定绑定解析](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryProxy/src/injection/injectors/tdai-fixed-asset.ts#L42-L122)，[当前 injector 注册事实](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryProxy/src/injection/index.ts#L277-L307)

### 3.6 Skill、Wiki、CodeGraph：名字相似但不是同一能力

- 对方 Skill 是版本化资产包：每个 `(skill_id, version)` 是不可变快照，有 head、content hash、manifest、active/archive 和 FTS；资源放在 `skills/<id>/v<version>/files`，有路径穿越防护及单文件 5 MiB、单 Skill 50 MiB 限额。[证据：Skill DDL](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/skill/skill-store-ddl.ts#L1-L97)，[资源存储与边界检查](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/skill/skill-resource-store.ts#L47-L129)，[容量限制](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/skill/skill-resource-store.ts#L165-L216)。本地 `mem crystallize` 的 Workflow capsule 是从经验结晶出的程序性知识，目前不是带文件 manifest 的通用包仓库；两者可互补，不能按同名视为已有同一功能。
- 对方 Wiki 图是从 BM25 seed 出发沿 `[[wikilink]]` 做 BFS 扩展，设 hop 衰减和最多 200 节点。[证据：Wiki graph search](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryKnowledge/src/engines/wiki/graph-search.ts#L1-L90)
- 对方 CodeGraph 是对 `@colbymchenry/codegraph` 的包装，用于 symbol/caller/callee/file 等代码结构查询。[证据：CodeGraph bridge](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryKnowledge/src/engines/code/bridge.ts#L1-L12)，[工具桥接](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryKnowledge/src/engines/code/bridge.ts#L89-L179)

本地 `mem` KG 表达 capsule/entity 事实关系、时态边和检索图信号；对方 Wiki/CodeGraph 表达文档链接和代码静态结构。二者问题域不同，不应拿对方的“KG”名称推断其拥有本地记忆实体 KG。

### 3.7 不同 Agent/工具下的 Skill 自进化与 MCP 自动化

对用户关心的“换 Claude Code、CodeBuddy 或其他 Agent 工具后能否自进化”，固定 SHA 中有一条真正可运行的 **tool-run → Skill 变更** 链路；但 **MCP tool schema 本身不会自生成、自修改**。两者要严格分开。

```text
Anthropic / OpenAI 工具循环
  → 五角色轨迹归一化
  → “真人一轮完成”时追加
  → 10 次 tool_call 或 40 KiB 触发归档
  → 持久 task + 按 team/agent 调度、串行抽取
  → Skill Review Agent 查旧 Skill 并 create/update/patch/files_write
  → 不可变 Skill 新版本
  → 新会话按 Agent/任务预装；配置 backing 时固定 first-seen 版本
```

| 环节 | 对方确实做了什么 | 自动化边界 / 对本地的启发 |
|---|---|---|
| 跨协议轨迹收集 | Proxy 分别解析 Anthropic/OpenAI，归一为 `user/assistant/tool_call/tool_result/system`，保留 `tool_call_id` 和 `tool_name`；只在无 tool call 的 final answer 到达时提交整个 human round，避免一次工具循环被重复记 N 次。[证据：round-level gate](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryProxy/src/skill/handler-glue.ts#L1-L25)，[五角色归一化](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryProxy/src/skill/normalize-conversation.ts#L1-L43)，[提交和软降级](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryProxy/src/skill/handler-glue.ts#L70-L167) | 这是跨 Agent tool 经验能够结晶的前提，值得直接借数据契约和 round boundary；不借 Proxy 强绑。 |
| 成本感知触发 | Core 按 `space/user/team/agent/session` 累积，默认累计 10 次 `tool_call` 或 40 KiB 才归档；大请求/超限块也会触发。[证据：计数契约和默认阈值](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/skill/conversation-add/add-handler.ts#L37-L99)，[触发判定](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/skill/conversation-add/add-handler.ts#L193-L265) | 不每轮调 LLM，可显著降成本和重复 Skill。功能受 `skill.extraction.enabled` 和 LLM runner 门控；库内解析默认为关，不是不配置也会无条件自启。[证据：显式 enable 与无 runner 降级](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/skill/skill-config.ts#L129-L190) |
| 队列和故障处理 | 先落 archive，再写 task/入队，避免 worker 读到幽灵任务；worker 有 Agent 级锁/续约、每轮公平配额、transient retry 和 permanent DLQ。[证据：archive-first](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/skill/conversation-add/trigger-service.ts#L1-L22)，[并发/重试/DLQ 契约](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/skill/conversation-add/extract-worker.ts#L1-L17)，[失败分类](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/skill/conversation-add/extract-worker.ts#L79-L113) | 可借“按 Agent 串行、跨 Agent 并行”和先 archive 后 enqueue。不应再建一个能直接打开 Lance dataset 的写进程；必须由唯一 `mem serve` writer 承接写入。 |
| Skill Review Agent | 每次抽取都先带入该 Agent 最近至多 5 个 Skill，prompt 强制区分 Skill/Memory/Wiki/CodeGraph/临时上下文、先 list/view 再写、允许 `Nothing to save`，并用结构化 tool calls 代替难解析的 JSON 文本。[证据：抽取和近期 Skill](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/skill/skill-extractor.ts#L112-L170)，[查重上下文](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/skill/skill-extractor.ts#L240-L270)，[分类/接受门](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/skill/prompts/skill-review-prompt.ts#L95-L149) | 这个“Review”是 LLM 自审，不是人工 review gate。本地应借分类、查重、no-op 和结构化输出，但只生成 `PendingConfirmation` 提案。 |
| Skill 写入和版本 | Review Agent 只拿到 2 个读工具和 4 个写工具，没有 delete/remove；但 create/update/patch/files_write **会当场调 SkillCore 持久化**，返回的 candidate 只是事后 audit receipt。更新使用 `expected_version`，新内容追加不可变版本。[证据：工具边界与直接写入](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/skill/skill-tools.ts#L1-L17)，[调用 SkillCore](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/skill/skill-tools.ts#L112-L207)，[乐观锁和新版本](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/core/skill/skill-core.ts#L302-L360) | 权限收窄值得借，直接激活不值得借。对方默认也不允许主 Agent 自由写 Skill，把特权留给隔离的 extraction path。[证据：主模型写入默认关闭](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryProxy/config.example.yaml#L562-L567) |
| 装载与会话一致性 | Skill catalog 在 session init 按 Agent/任务描述做 BM25 预装并会话缓存。配置 pin backing 时，首次 search/get 见到的 Skill 版本会固定：Redis 路径用 HSETNX，TTL 可配、默认 30 分钟；ProxyStorage 路径用 `nottl` + `putTextIfAbsent`；两者都未开启则不 pin。会话内成功写新版本时才更新 pin。[证据：Skill 预装](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryProxy/src/injection/injectors/skill-injector.ts#L162-L256)，[backing 分支](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryProxy/src/skill/skill-bridge.ts#L95-L122)，[lazy pin 语义](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryProxy/src/skill/skill-bridge.ts#L787-L875)，[Redis HSETNX/TTL](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryProxy/src/skill/version-pin-repo.ts#L1-L93)，[ProxyStorage CAS/nottl](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryProxy/src/skill/kv-version-pin-repo.ts#L1-L18) | 强烈建议借会话版本固定：Skill head 在长任务中升级，不应让同一 Agent 执行到一半换规则。 |
| MCP 和资产反思 | MCP 明确是 12 个静态、只读查询工具，创建/删除/同步留在管理面；HTTP `/tools/list` + `/tools/call` 能按 Wiki/CodeGraph 资源自发现，但 registry 仍是静态白名单。[证据：MCP 静态查询面](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryKnowledge/src/mcp/tools.ts#L1-L25)，[HTTP 自发现与白名单](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryKnowledge/src/routes/tools.ts#L180-L295) | 借“MCP 查询数据面 / HTTP 管理面”和 meta-tool discovery，不做动态 MCP 生成。`asset_reflection` 只是可选 prompt，让模型在最终答案自述工具是否有用，并没有持久反馈或改排序，不能称为自进化。[证据：reflection 仅为 prompt](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryProxy/src/injection/injectors/asset-reflection-injector.ts#L1-L63) |

因此，对本地 `mem` 的正确借法不是“让 MCP 自己长出新 tool”，而是：把不同 Agent 工具的已完成 round 变成统一、可追溯的经验输入；再用确定性规则触发结晶，让 LLM 只编译出待审 Skill 提案；审核通过后才生成不可变 Bundle 版本，并在正在进行的会话内 pin 版本。

### 3.8 本地 `mem` 基线与审阅中确认的边界

为避免只用项目文档互相对照，以下判断落到本地当前源码：

- capsule 已有类型、五态生命周期、四级 scope、visibility、verbatim `content`、显式版本前驱和 hard expiry。[domain](../src/domain/capability_capsule.rs#L9)，[ingest request](../src/domain/capability_capsule.rs#L181)
- 默认 Lance 后端之外，统一 `Backend` 组合 capsule、search、embedding job/vector、KG、transcript、entity、session、maintenance、mine cursor、evolution 等持久化能力；并非单一向量表。[backend](../src/storage/backend.rs#L31)
- capsule 与 transcript 有独立的 durable embedding job 状态机（`pending → processing → completed | failed | stale`）、lease 回收和批量 worker；这些已是腾讯 Skill 队列模式可复用的本地基础，不需要再造一套通用队列。[embedding job contract](../src/storage/embedding_job_store.rs#L1)，[capsule worker](../src/worker/embedding_worker.rs#L40)，[transcript worker](../src/worker/transcript_embedding_worker.rs#L57)
- transcript 的一级边界是 tenant/caller_agent/session，`cwd` 和 `git_branch` 是 `meta_json` 中的 envelope metadata，不等于 capsule 的四级 scope。[conversation message](../src/domain/conversation_message.rs#L4)
- 检索实际走 Tantivy BM25 + ANN + RRF，再叠加 scope、intent/type、confidence、freshness、decay、graph、lifecycle 等信号；语义超时或建索引期间软降级为 BM25-only。[search orchestration](../src/service/capability_capsule_service.rs#L1738)，[ranking](../src/pipeline/retrieve.rs#L663)
- Claude Code hook 和 Pi adapter 都会在 Agent turn 前注入；默认 hook 先完成 capsule recall，再 best-effort 查 transcript，index 风格只注入 headline + id，需要时显式取 verbatim 正文。[hook](../src/cli/hook.rs#L244)，[progressive-disclosure formatter](../src/cli/hook.rs#L335)
- 本地已有 feedback 调权、expiry/decay/idle archive/near-dup；evolution 中 generalize/workflow-generalize/refine/split 生成 `PendingConfirmation`，而 merge 经 K gate 后可直接归档 loser，reweight/co-recall 也是自动写，后三者不是人工 review-gated。这些自动算子仍有防抖、审计、可选 reranker veto 或回滚机制。`mem crystallize` 生成的是 Workflow capsule，不是带资源 manifest 的可安装 Skill 包。[feedback domain](../src/domain/capability_capsule.rs#L67)，[evolution worker](../src/worker/evolution_worker.rs#L70)，[crystallize](../src/cli/crystallize.rs#L1)
- KG 是记忆实体图，边含 `valid_from/valid_to` 和强化/衰减字段；但当前 `graph_edges` schema **没有 tenant**，源码明确说明所有 tenant 共用一张图。这是后续多租户资产治理前必须处理的本地安全边界。[GraphEdge / GraphStats](../src/domain/capability_capsule.rs#L366)

另有一个与本次“版本链”比较直接相关、但不是腾讯设计带来的发现：单条 ingest 正确保留请求的 `supersedes_capability_capsule_id`，batch `prepare_one` 却无条件写为 `None`，会让批量写入丢失版本前驱及对应图边。[单条路径](../src/service/capability_capsule_service.rs#L459)，[batch 路径](../src/service/capability_capsule_service.rs#L656) 这应作为独立 bug 修复，不计入“借鉴腾讯”的收益。

## 4. 能力对比表

| 能力维度 | 本地 `mem` 现状 | TencentDB Agent Memory 做法 | 可借鉴度 | 借鉴建议 |
|---|---|---|---|---|
| 产品边界 | self-hosted memory/capsule 服务；HTTP/MCP 与存储解耦 | Memory Hub，覆盖 Chat Memory、Skill、Wiki、CodeGraph、Panel 和 Proxy | 中 | 保留专注边界，只吸收资产治理和上下文契约，不扩成一体化 Agent 平台 |
| 原始会话 | 独立 transcript archive、embedding jobs、HTTP 搜索；读错误软降级 | 独立模式下 L0 落 SQLite/向量表并按日追加 JSONL；服务模式可走 StorageAdapter/COS 与 TCVDB | 低 | 不复制双写形态；可参考 scope 字段，不改变 transcript 的独立状态与故障边界 |
| 原子记忆 | 类型化 capsule，content 是 verbatim 事实源，有 summary 索引提示 | L1 为 LLM 提取/改写的原子记忆，含 7 类工作/人物记忆 | 中 | 可借其类型样本完善 miner taxonomy；不能让生成式 L1 替代 verbatim capsule |
| 层级记忆 | 胶囊类型 + episode/KG/`crystallize` 产出的 Workflow；没有显式 L2 场景导航和 L3 用户画像层 | L0 raw → L1 atom → L2 scene → L3 persona；上层优先，下层回查 | 高 | 增加只读派生的 Scenario Navigator；画像需单独 consent/review，不直接自动上线 |
| 业务 scope | capsule 有 tenant + Global/Project/Repo/Workspace + source agent；transcript 的一级边界是 tenant/caller_agent/session，cwd/git_branch 在 `meta_json`；KG 仍 tenant-less | team/agent/user/session/task 多维 scope；L2/L3 刻意 team+agent 聚合 | 高 | 扩展 consumer-agent 维度前，先补通用 visibility 授权与 KG tenant 边界 |
| 团队/主体模型 | 无一等 user/team/agent registry | users/teams/agents/tasks/assets 都是一等表 | 高 | 为多 Agent 引入最小主体模型和稳定 agent identity |
| ACL | Visibility 目前是持久化元数据，通用 search/get 未按主体授权；没有 asset permission | user/team_role/agent ACL；owner、private、restricted 和 bind 权限有服务端检查 | 高 | Loadout 上线前先建立可信 caller identity 和统一 authorization seam；所有后端 schema/builders/parsers 同步改动 |
| Agent Loadout | 调用方按 query/scope 主动召回；无固定资产清单 | agent↔asset 绑定保存 injection_mode/priority；Proxy 已消费 `chat_memory` 绑定加载跨 Agent L2/L3，但通用 injection-mode executor 未完整接通 | **高** | 实现 Loadout-lite 时借领域模型，不照抄其未完全统一的执行层 |
| 检索通道 | Tantivy BM25 + Lance ANN + KG，RRF 后再综合 scope/type/confidence/freshness/decay 等 | SQLite/TCVDB dense+BM25，RRF k=60；TCVDB 可原生 hybrid | 低 | 本地更强；仅参考统一 store capabilities 与后端 native hybrid fast path |
| 排序信号 | lexical/semantic/scope/intent/confidence/freshness/decay/graph/feedback | 审阅路径主要是 dense、BM25 和 RRF；未发现反馈、衰减、图进入 L1 最终分值 | 低 | 不迁移排序；用当前评测验证新增 Loadout prefilter 是否损害 recall |
| 查询失败降级 | semantic timeout/index rebuild/BM25 fallback；transcript 读边界软降级 | embedding/TCVDB 出错时回退或返回空；SQLite 双路并行 | 中 | 可借统一 capability/fallback 可观测性，但必须保留本地更严格的 transcript 软降级不变量 |
| Prompt 注入 | retrieval/compress 返回 context pack；调用方/MCP formatter 负责使用 | 稳定 L2/L3、动态 L1、按需工具三通道；system/user 分位和预算明确 | **高** | 把三通道变成稳定 API/MCP DTO，不在服务内硬编码某个模型 prompt |
| 更新/去重 | near-dup 可提 `suspected_supersede`；generalize/refine/split 是待审提案，merge/reweight/co-recall 属自动算子 | 每条新记忆各自向量/FTS 取 top-K，合成统一候选池后用一次 LLM 逐条判 store/skip/update/merge | 中 | 可借统一候选池降低 LLM 调用数，但要保留 propose-only 和 `supersedes_capability_capsule_id`，禁止直接删旧事实 |
| 生命周期 | PendingConfirmation/Provisional/Active/Archived/Rejected；expiry、decay、auto-promote | L1 未发现等价状态机；Skill 有 active/archive；资产有管理状态 | 低 | 本地明显更强，不照搬；未来让 Loadout 只绑定可见且允许的生命周期状态 |
| 反馈闭环 | useful/applies/outdated/not-apply/incorrect 即时调 confidence/decay/status | 未发现 recall-result 用户反馈进入 L1 排序；`asset_reflection` 仅让模型在答案里自述有用与否 | 低 | 保留现有闭环；可以用真实 tool call/result 和任务成败生成弱信号，不用模型自评直接调权 |
| 遗忘 | decay、expiry、idle archive、版本状态；不改写 verbatim content | 可选按年龄硬删除 L0/L1，默认关闭，有最小保留数 | 低 | 不复制 hard-delete 作为语义遗忘；仅可做经 retention policy 授权的物理合规清理 |
| 压缩 | 对 verbatim capsule 做按 token budget 的输出压缩，存储事实不变 | L1/L2/L3 是生成式逐层提炼；另有消息 offload | 中 | 场景层只能是可重建派生视图；必须保留来源 capsule/message id 和显式 provenance |
| 版本审计 | capsule `version` + `supersedes_capability_capsule_id`；但 batch ingest 当前会丢前驱 | L1 update/merge 删除查询存储旧项；Skill 则有不可变版本快照/head | 中 | 先独立修本地 batch bug；L1 方式不借，Skill immutable snapshot/head/manifest 值得借给 Workflow bundle |
| 跨工具 Skill 自进化 | transcript/mine/evolution/`crystallize` 都有，但没有“完成工具 round 自动转 Skill”的持续链路 | 两种 LLM 协议归一化 tool call/result，round 结束时累积，达阈值后按 Agent 排队抽取并自动 create/update Skill | **高** | 借轨迹契约、触发、队列、查重和 no-op；改成 `PendingConfirmation` 提案，不让 LLM 直写 Active |
| Skill 结晶/装载 | `crystallize` 合成 Workflow capsule，默认 dry-run，accept 才写；无资源包和会话版本 pin | Skill 是带资源文件/manifest 的不可变 bundle；按 Agent/任务预装，配置 pin backing 时会固定会话首次见到的版本 | 高 | 在 Workflow 之上增加 bundle manifest 和 session pin，不复制成第二套记忆事实库 |
| MCP 自动化 | MCP 是 stdio → HTTP forwarder，工具面受服务端契约管理 | 12 个静态、只读 MCP query tools；HTTP 可按资源发现静态白名单工具，没有 MCP schema 自生成/自修改 | 中 | 借数据面/管理面分离和 `tools/list` meta-tool；不建自修改 MCP registry |
| KG | 记忆实体关系、时态边、BFS 和检索信号 | Wiki wikilink 图 + 第三方代码结构图 | 低 | 作为外部知识工具接入即可，不能替换本地 entity KG |
| 工具渐进披露 | MCP 提供 memory tools；已有按需查询，但未建统一 tool-ref context lane | session 先注入资源/工具清单，再按需 `/v3/tools/call` | 高 | 为 Wiki/CodeGraph/Skill/大型 capsule 返回 tool reference，正文延迟加载 |
| 后端部署 | Lance 默认；Postgres/ClickHouse 可选；单 writer 强约束 | SQLite 独立模式；TCVDB + MongoDB；云模式还依赖未完整开源的 integrations | 低 | 不迁移后端；借鉴应停留在领域/API 层，避免把云产品耦合带进本地服务 |
| 测试与可验证性 | Rust 单元/集成测试，CI 要求 fmt/clippy；另有 IR golden/ablation/LoCoMo/LongMemEval（后两者仅检索 recall） | 有 Vitest 配置和 metadata backend contract suite，但固定 SHA 未见注释所称的 SQLite/MongoDB runner；PersonaMem runner/dataset 也未公开 | 低 | 不接受 README benchmark 作为回归门；所有借鉴点先补本地测试和离线 eval |

## 5. 最值得落地的 5 项

### 5.1 P0：可信 Agent identity、授权边界与 Loadout-lite

**具体做法**

- 先定义可信的 `agent_id`（consumer identity）来源；不能把请求体里可任意伪造的 `caller_agent` 当鉴权。单机可信 MCP/CLI 可由服务端配置映射，网络调用则需认证层解析 identity。
- 新增 `agent_asset_bindings`，绑定目标第一阶段只允许 capsule id、保存的检索策略或 tool reference。
- 字段至少包含 tenant、agent_id、asset_ref、`injection_mode = stable | dynamic | tool_only`、priority、enabled、created/updated；唯一约束防重复绑定。
- retrieval 前先解析 loadout：固定 directive/workflow 可以进入 stable lane，普通 capsule 作为动态候选 boost，超大资源只提供 tool ref。
- 当前 capsule 没有可信 owner/principal，`source_agent` 不能代替 owner。因此 Phase 0 只允许可信 admin 绑定 `Shared/System` 资产，`Private` 明确禁止进入 Loadout；Phase 1 先增最小 `owner_subject/principal` 语义并让 search/get 执行 tenant + visibility + lifecycle，Phase 2 再增完整 user/team role ACL。Loadout 只是“装配”，不是绕过权限的白名单。

**改动范围**：`src/domain` 新主体/绑定类型；`src/storage` 各后端 schema、batch builder、parser 和 repository；`src/http`/MCP 管理接口；`pipeline/retrieve.rs` 预过滤/boost；metrics；集成测试。Lance 表必须按项目不变量同步修改 schema、builder、parser。

**成本估计**：Phase 0 可信 identity + Shared/System binding 共 6–10 人日；增加 owner/principal 与 Private 授权再需 6–10 人日；完整团队 ACL/Panel 另需 15–25 人日。

**收益与风险**：这是对方最强、且本地确实缺失的能力；能让不同 Agent 获得稳定、可解释的能力装配。主要风险是 ACL 越权和过度 boost，必须在服务端校验 tenant/visibility/lifecycle，并对绑定和普通 recall 做离线对比。

### 5.2 P0：稳定 / 动态 / 工具引用三通道上下文契约

**具体做法**

- 在现有 context pack 上增加结构化输出：`stable_context`、`dynamic_memories`、`tool_references`，并分别附 token budget、provenance 和 capsule ids。
- stable lane 只放经 review 的 directive/workflow、Loadout 固定项和场景导航；dynamic lane 保持当前 query ranking；tool lane 只放名称、摘要、版本和 fetch/call 参数。
- MCP formatter 和 HTTP client adapter 决定最终 system/user 放位；服务端不绑定某一 LLM 的 prompt 模板。
- 保持 `memories.content` verbatim；任何摘要只是索引或展示提示。

**改动范围**：`src/pipeline/compress.rs`、retrieve/service response DTO、HTTP/MCP serialization、CLI/banner formatter 与 round-trip tests。

**成本估计**：4–7 人日。

**收益与风险**：能最快获得对方 progressive disclosure 的价值，并为 Loadout 和场景导航建立接口。风险是响应兼容性，宜新增版本化字段并保留现有返回。

### 5.3 P1：跨 Agent 工具 round → 待审 Skill 提案编译器

**具体做法**

- 先在 transcript 索引层定义统一 `completed_tool_round`：包含 tenant/session/caller-agent、消息 ids、`tool_call_id`、tool name、参数摘要、result/成败、最终答复和来源 adapter。存储层仍保留原始 transcript verbatim，这是可重建索引记录。
- 定义确定性触发器：例如“至少 3 次工具调用 + 有最终成功证据”、累计 token/字节阈值、同类任务重复出现；把候选写入 durable queue，以 tenant/agent 串行去重，不每轮调 LLM。
- 保持常驻 `evolution_worker` 的确定性/无 LLM 边界：它只找候选、固化 provenance。LLM 编译放进显式开启的一次性 `crystallize` lane 或受控任务，输出结构化 Skill/Workflow proposal。
- 编译 prompt 借对方的五分类、read-before-write、高质量门和 `Nothing to save`；但 LLM 只拥有 `list/view/propose`，没有 create/update/delete Active 资产的权限。
- 提案以 `PendingConfirmation` Workflow 或新 SkillBundle candidate 落地，记录来源 transcript/message/tool-call ids、规则/模型版本、查重命中和审核决定；接受后再建版本链。

**改动范围**：`src/domain` 增候选/来源 DTO；`src/service/transcript_service.rs` 或新 `pipeline/skill_candidate` 做 round 识别；新 durable job store/worker；扩展 `src/cli/crystallize.rs`；review HTTP/MCP API；metrics 与 golden eval。任何 transcript 新读路径都必须在 Lance 错误时软降级。

**成本估计**：最小“round detector → durable candidate → `PendingConfirmation` Workflow” 8–12 人日；跨 adapter 完整归一化、DLQ 和 SkillBundle 产物再需 6–10 人日。

**收益与风险**：这是用户所问“换不同 Agent 工具也能自进化”的核心。最大风险是轨迹里的 prompt injection、密钥/主机信息被结晶以及偶然成功被误当通用规则；需要秘密扫描、参数化、可复用性评分和人审一起成为接受门。

### 5.4 P1：Scenario Navigator 派生视图

**具体做法**

- 从 Active capsule、episode 和 KG 聚类生成 `scenario_view`，只保存 scenario id/name、summary、source ids、heat、updated_at 和 version，不保存新的“事实正文”。
- 召回时先返回少量高 heat/高相关场景导航，Agent 或客户端选择后再 fetch 对应的 verbatim capsules/transcript range。
- heat 可由使用次数、最后召回和有用反馈派生；重建失败不影响主检索，视图可删除重算。
- L3 persona 暂不跟进。画像有隐私、错误固化和跨用户污染风险，若以后实现，应独立 consent、review 和 scope。

**改动范围**：新 `pipeline/scenario` worker；storage 派生表和重建任务；HTTP/MCP browse/fetch API；compress/tool-ref lane；metrics/eval。

**成本估计**：8–15 人日；若加入 LLM 聚类与增量重建，再增加 5–8 人日。

**收益与风险**：对 806 条及继续增长的胶囊，可减少每轮候选正文注入。风险是摘要被误当事实，因此 API 和 prompt 必须明确“导航提示，不是事实源”。

### 5.5 P1：版本化 Skill Bundle + 会话版本固定

**具体做法**

- 保持 Workflow capsule 作为能力语义和 provenance 锚点，另建不可变 `skill_bundle_version` 与 `resource_manifest`；head 指向当前版本。
- manifest 支持脚本、模板、示例、参考文档，记录 path、size、sha256、media type；资源使用内容寻址或安全目录存储。
- 加路径规范化、防目录穿越、单文件/总包大小限制；上传和切换 head 必须鉴权，旧版本可审计。
- 加入 session-scoped first-seen pin：首次装载时记住 `(skill_id, version)`，同一会话后续读取默认指向该版本；只有当本会话审核并接受了新版本时才更新 pin。本地可先用 server-side session 存储，不强制引入 Redis。
- `crystallize --accept` 可选择生成 bundle proposal，但不自动执行 bundle 中的脚本。

**改动范围**：`src/domain`、storage 新表/对象存储抽象、session version-pin store、`src/cli/crystallize`、HTTP/MCP upload/get/list、校验和安全测试。

**成本估计**：Bundle 10–18 人日，session pin 3–5 人日；若做跨实例导入导出和签名，再增加 5–10 人日。

**收益与风险**：把“会做什么”的 Workflow 扩展成“带什么材料执行”，并保证长任务内行为一致，但不复制一套独立事实库。风险集中在任意文件写入和供应链，必须限制路径、类型、体积，默认不可执行。

## 6. 明确不建议照搬

1. **不整体复制 L0-L3 替代 capsule 模型。** 对方层级是生成式物化视图，本地 capsule 有更强的 verbatim、状态机、版本链、反馈、衰减和 KG 语义。只把 L2-like 导航做成可重建派生层。
2. **不照搬 LLM `update/merge` 后删除旧 L1。** 这会弱化审计、回滚和错误恢复。应转成 proposal，接受后建立 `supersedes_capability_capsule_id`。
3. **不把按年龄硬删除当作记忆遗忘。** 本地 decay、expiry、idle archive 更符合语义遗忘；物理删除只能用于明确 retention/compliance policy。
4. **不复制 SQLite + 每日 JSONL 的双重持久化。** 对方以 JSONL 作恢复源、SQLite 作查询引擎有其本地插件背景；本地已经围绕 Lance 强一致读、异步 embedding job 和多后端形成不变量，增加第二事实源会扩大一致性面。
5. **不把 MemoryProxy 变成 mem 的强制入口。** 本地 HTTP/MCP 解耦更适合 self-hosted 与多调用方。可借注入 slot/contract，不应让所有 LLM 流量必须经过 MITM proxy。
6. **不以 Wiki/CodeGraph 替换本地 KG。** 前者是文档链接图和代码静态图，后者是记忆实体事实与时态关系；可以作为外部知识工具并列接入。
7. **不按完整腾讯云 service mode 设计本地架构。** 当前开源 SHA 缺少 Redis/COS integrations 的可运行实现，且本地目标是 self-hosted；领域模型必须与 TCVDB/COS 解耦。
8. **不直接采用 README benchmark 数字做决策。** README 给出 PersonaMem 成绩，但固定 SHA 未发现对应 benchmark dataset、runner 或 tracked 测试，当前只能视为项目方陈述。[证据：README benchmark 表](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/README.md#L248-L254)
9. **不让 Skill Review Agent 直接写 Active 能力。** 对方的 candidate 是写入后的 audit receipt，而不是写入前等人复核的提案。本地需反过来：LLM 只能 propose，审核接受后才改活跃能力。
10. **不做自修改 MCP registry。** 对方 MCP 也是静态查询白名单；自动生成 tool schema 会把审权、兼容性和 prompt-injection 风险变成运行时问题。
11. **不用模型的“资产反思”文本直接调权。** 这只是自报，可用于调试，不能代替真实 tool event、成功/失败结果和调用方 feedback。

## 7. 已证实、未发现与未来计划边界

### 已由固定 SHA 源码证实

- L0-L3 的字段、scope、提取、导航和存储实现；SQLite/TCVDB hybrid retrieval；RRF `k=60`。
- 用户/团队/Agent/任务/资产 schema、固定绑定、注入模式、优先级和 ACL 服务端检查。
- Core hook 与 Proxy 的注入策略并不完全一致；不能用单一描述概括所有 adapter。
- Skill 有真正的不可变版本、head、manifest、资源文件和边界校验。
- Proxy 会在跨协议 human round 完成后累积工具轨迹，达阈值后自动触发 Skill Review Agent；该 Agent 能直接创建/更新 Skill，不是人工 review gate。
- Skill 列表按 Agent/任务预装；配置 Redis 或 ProxyStorage pin backing 时，有 session-scoped first-seen version pin，无 backing 时不 pin。
- Wiki 图、代码图与 memory entity KG 是不同类别的图。

### 本次源码审阅未发现，不能声称对方已有

- PostgreSQL 记忆后端。
- L1 recall 中与本地等价的反馈调权、confidence/decay/freshness/graph 综合排序。
- L1 与本地等价的 `supersedes_capability_capsule_id` 在线可审计版本链。
- 一致性哈希或跨节点物理 shard 算法。
- MCP tool schema/registry 根据使用经验自生成、自修改或自部署。HTTP `tools/list` 只是对静态资源工具的自发现。
- `asset_reflection` 的模型自述被持久为 feedback，或用于自动调整 L1/Skill 排序。
- 固定 SHA 中可运行的完整 Redis/COS integrations。
- 与 README PersonaMem 数字对应的 benchmark 数据集/runner。仓库有 Vitest 配置及 `metadata-store.contract.ts` 共用契约套件，但固定 SHA 未见该文件注释所称、负责实际调用套件的 SQLite/MongoDB `*.test.ts` runners，不能据此推断 CI 可直接执行这些契约测试。[contract suite 注释与入口](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/MemoryCore/src/metadata/store/metadata-store.contract.ts#L1-L45)

### 明确是 Roadmap，不应写成当前能力

项目 Roadmap 把用户/团队自定义 prompt 与 provenance、Skill export、Codex Plan 支持、Wiki 并发构建列为后续项。[证据：Roadmap 规划](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/ROADMAP_CN.md#L13-L106) README 也承认 Wiki/CodeGraph 构建仍是异步、私有/SSH 仓库支持不完整、全自动记忆路由还在迭代。[证据：当前限制](https://github.com/TencentCloud/TencentDB-Agent-Memory/blob/4dca55c41bf11cb19b49728dbe495c8e05d25abb/README_CN.md#L256-L261)

## 8. 推荐落地顺序

推荐先做 **可信 Agent identity + authorization seam + Loadout-lite**，而不是先做 L2/L3 提炼。原因是：

- 它补的是当前多 Agent 场景中的治理缺口，而不是重复已有检索能力；
- Phase 0 可以复用 tenant、lifecycle 和现有 capsule id，但只允许可信 admin 装配 Shared/System；Private 必须等 owner/principal 和真实 Visibility 授权检查完成后再开放；
- 它为后续 stable/dynamic/tool-only 三通道提供真实的配置来源；
- 场景导航、待审 Skill 提案、Skill Bundle 和外部知识工具都能自然成为 Loadout 的 asset target。

建议以一个窄切片验证：由服务端可信配置识别 Agent，先支持“某 agent 由可信 admin 固定绑定 1–20 个 Active 且 Shared/System 的 directive/workflow capsule”，Private 直接拒绝，且仅影响 MCP context-pack 输出；不改通用 search 排序，不做 UI，不立即引入完整 user/team RBAC。验收至少包括伪造 caller identity、tenant 越权、Private/Archived/Rejected 不可绑定、token budget、重复绑定幂等、解绑即时生效、与无 Loadout 的 recall 回归对比。验证通过后再增加 owner/principal、Private 授权、tool reference、scenario view 和主体 ACL。

如果当前只排“自进化”这条专项，则第一个应落的是 **completed tool round → durable candidate → `PendingConfirmation` Workflow** 的最小闭环，而不是 MCP tool 自生成。它可以直接复用现有 transcript、review lifecycle 和 `crystallize` 语义，又不破坏 verbatim 和单 writer 不变量。

## 9. 调研方法与限制

- 使用 `git ls-remote --symref` 锁定默认分支，并将官方仓库浅克隆到独立 `/tmp` 目录；所有对方源码引用均固定到 commit `4dca55c41bf11cb19b49728dbe495c8e05d25abb`，避免分支后续移动导致证据漂移。
- 证据只取自官方仓库 README、docs、source、配置和 schema；没有使用二手博客或按项目名称推测实现。
- “未发现”表示在上述固定快照的 tracked 文件及目标流程中未检出，不等同于证明腾讯内部或未公开组件绝对不存在。
- 本地比较基于当前源码和项目架构约束；没有在本次外部调研中写数据、启动服务或验证部署状态。
- 本报告没有创建或修改 Vikunja 任务；是否关联既有任务线由项目负责人决定。
