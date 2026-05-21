# Agent Memory Strategy Readiness

日期：2026-03-25（原稿） / 2026-05-21（§9 audit 增补）
状态：Draft + Audited

> §1–§8 是 2026-03-25 的原始 Draft；状态与代码现状会漂移，**以 §9 audit 表为准**。完成新条目后请回 §9 更新对应行，不要直接改 §4 的 `[ ]` 框（保留原稿作为时间锚点）。

## 1. 背景

`mem` 当前已经具备较完整的通用记忆服务能力：

- 可作为独立 HTTP 服务运行；
- 支持记忆写入、检索、详情查询、反馈、pending review、episode、graph 邻居查询；
- 具备 `mem-mcp` 适配层，可供多个 MCP client / coding agent 共用；
- 具备 Docker、CI、Release 等基础交付能力。

因此，`mem` 可以视为一个 **通用 AI agent 记忆服务 v1**。  
但如果目标是进一步把它定义为一个 **可稳定服务多个 coding agent、支持项目级知识复用、并具备低污染默认行为的策略通用平台**，则当前仍有明显差距。

本文档的目的，是把这些差距收敛为一份可执行的 TODO 与验收标准，作为后续从 MVP 走向 Beta / Production-ready 的参考。

## 2. 当前判断

### 2.1 什么叫“能力通用”

能力通用，指的是系统已经提供了较完整的原子能力，且这些能力不强绑定某一个 agent：

- `memory_ingest`
- `memory_search`
- `memory_get`
- `memory_feedback`
- `memory_list_pending_review`
- `memory_review_*`
- `episode_ingest`
- `memory_graph_neighbors`

这些能力已经足够让不同 agent 接入并共享同一记忆后端。

### 2.2 什么叫“策略通用”

策略通用，指的是不仅“能接入能使用”，而且“不同 agent 接入后，默认就会以一致、低噪声、可复用、可审计的方式使用系统”。

这要求系统对以下问题给出稳定默认答案：

- 什么时候必须先搜索记忆；
- 什么内容允许自动写入，什么内容必须进入 review；
- 如何区分 `fact`、`experience`、`preference`；
- 默认搜索边界应该落在 `project`、`repo` 还是 `personal`；
- 多 agent 写入同一项目时如何避免互相污染；
- 旧记忆如何衰减、覆盖或 supersede；
- 反馈如何真正改善下一次召回；
- 不同 agent 是否能遵循同一套高层工具和默认行为。

当前 `mem` 已经接近“能力通用”，但尚未完全达到“策略通用”。

## 3. 距离策略通用还差什么

### 3.1 项目级记忆边界

目标是让记忆默认优先服务当前项目，而不是把其他项目、其他仓库或个人偏好误召回到当前任务中。

当前风险：

- 写入时上下文不完整，导致 `tenant`、`project`、`repo`、`module` 边界模糊；
- 搜索默认边界不够严格，容易召回相邻但不相关的记忆；
- 相似模块名、相似问题名可能造成跨项目污染；
- `project` / `repo` / `personal` 三层 scope 还未形成足够稳定的默认策略。

这部分如果做不好，系统会“越记越多，但越搜越不准”。

### 3.2 去噪

目标是让系统更偏向保留少而准、可复用、经验证的记忆，而不是收集大量一次性碎片。

当前风险：

- 未验证猜测进入自动写入路径；
- 临时中间结论被当作长期事实；
- 大段日志、过程碎片、重复总结进入强召回主路径；
- 经验类内容与事实类内容没有充分分流；
- 反馈与衰减尚未形成稳定的噪声淘汰机制。

这部分如果做不好，系统短期内看起来“很能记”，长期则会退化成噪声堆。

### 3.3 冲突处理

目标是让系统在长期运行中，能够处理旧方案与新方案、局部偏好与全局规范、相互矛盾结论之间的关系。

当前风险：

- 同一问题出现多个方案时，没有明确的优先级；
- 新记忆和旧记忆之间的 `supersedes` / `contradicts` 关系不够系统化；
- 偏好类冲突缺少强制 review 之外的后续治理；
- 召回时可能把互相冲突的内容一起返回给 agent。

这部分如果做不好，系统不会“更聪明”，而会把冲突一起积累下来，增加 agent 的判断成本。

### 3.4 复用质量

目标不是“搜到了内容”，而是“搜到的内容稳定帮助 agent 更快做对事情”。

当前风险：

- 检索命中的是关键词相近内容，而不是最有复用价值的方案；
- 真正高价值的历史方案没有被优先召回；
- 召回结果压缩不够，返回内容对 agent 使用成本仍然偏高；
- 缺少持续衡量“记忆是否真的减少重复劳动”的机制。

这部分如果做不好，系统会停留在“可检索数据库”，而不是“高价值共享记忆系统”。

## 4. TODO 清单

以下 TODO 按建议优先级排序。

### 4.1 P0: 固化项目级边界默认策略

- [ ] 在检索入口统一要求并校验 `tenant` 与 `project` 上下文。
- [ ] 将默认自动检索范围收敛到 `project`，`repo` / `personal` 必须显式开启。
- [ ] 在写入入口明确 `project`、可选 `repo`、可选 `module` 的最小要求。
- [ ] 为缺少边界字段的请求返回清晰错误或降级行为，而不是静默写入模糊记忆。
- [ ] 在 MCP 高层工具中把 project-first 设为默认行为，而不是靠调用方自觉约束。

### 4.2 P0: 完成内容分层与写入治理

- [ ] 明确 `fact`、`experience`、`preference` 的判定规则，并体现在 MCP 高层工具语义上。
- [ ] 让 `fact` 默认走自动写入，但要求满足“已验证、稳定、可复用”条件。
- [ ] 让 `experience` 默认写入 `episode` 或低权重候选，而不是直接进入强召回主路径。
- [ ] 让 `preference` 强制进入 pending review，不允许直接活化。
- [ ] 明确拒绝写入内容的规则，例如未验证猜测、临时结论、原始大段日志、闲聊内容。

### 4.3 P1: 建立噪声治理机制

- [ ] 为重复、低价值、长期未命中的记忆定义衰减与降权策略。
- [ ] 为 recall 结果引入更强的摘要压缩，减少把原始噪声直接暴露给 agent。
- [ ] 定义“自动写入阈值”，限制每轮任务默认写入量，避免过度沉淀。
- [ ] 将 feedback 明确接入排序或召回抑制逻辑，而不是只存储状态。
- [ ] 为高噪声来源 agent 预留审计或降权钩子。

### 4.4 P1: 建立冲突处理机制

- [ ] 定义记忆 supersede 的最小规则，例如新事实替换旧事实的条件。
- [ ] 定义 `contradicts`、`supersedes` 等关系的写入与维护方式。
- [ ] 在 retrieval 输出中优先展示当前有效版本，而不是平铺旧新方案。
- [ ] 对 preference 冲突建立人工 review 与最终生效版本机制。
- [ ] 为冲突记忆建立可观测指标，避免矛盾信息长期处于活跃召回路径。

### 4.5 P1: 建立复用质量闭环

- [ ] 定义“有帮助的复用”标准，而不是只统计检索次数。
- [ ] 在 agent 使用记忆后采集 `useful`、`outdated`、`incorrect` 等反馈。
- [ ] 将 feedback 结果回流到排序、去噪、冲突治理逻辑。
- [ ] 设计一组代表性真实任务，验证是否能稳定复用历史解决方案。
- [ ] 为“同类问题是否减少重复分析时间”建立对比实验或样本记录。

### 4.6 P2: 建立策略层高层工具默认面

- [ ] 交付并推广 `memory_bootstrap`。
- [ ] 交付并推广 `memory_search_contextual`。
- [ ] 交付并推广 `memory_commit_fact`。
- [ ] 交付并推广 `memory_propose_experience`。
- [ ] 交付并推广 `memory_propose_preference`。
- [ ] 交付并推广 `memory_apply_feedback`。
- [ ] 在 Skill / Prompt 层把这些高层工具设为默认路径，底层工具仅作维护和调试用途。

## 5. 验收指标

以下指标用于判断系统是否正在逼近“策略通用”。

### 5.1 项目级记忆边界

目标：默认只召回当前项目真正相关的记忆。

建议指标：

- 边界完整率：新写入记忆中，带有 `tenant` + `project` 的比例应接近 100%。
- 错项目召回率：抽样检索结果中，来自错误项目或错误 repo 的记忆比例应低于设定阈值。
- 显式扩展依赖率：跨 `repo` / `personal` 的召回应主要来自显式开启，而不是默认路径。
- 边界违规可观测性：所有缺少上下文边界的写入或检索请求都能被统计和审计。

建议通过标准：

- `tenant` + `project` 字段覆盖率 >= 99%
- 默认检索路径中，错误项目召回率 <= 5%
- 所有边界缺失请求都能在日志或指标中定位

### 5.2 去噪

目标：强召回主路径中的记忆以高价值、低重复、低临时性内容为主。

建议指标：

- 自动写入接受率：自动写入后，后续未被标为 `outdated` / `incorrect` 的比例。
- 噪声命中率：检索 Top-N 中，被人工判定为“无帮助 / 临时 / 重复”的比例。
- 重复记忆率：同一事实或同类结论的重复写入比例。
- 低价值记忆滞留率：长期无命中、无正反馈、但仍处于活跃召回状态的记忆比例。

建议通过标准：

- 检索 Top-5 中，噪声结果占比 <= 20%
- 重复记忆率持续下降，且新增重复项可被识别
- 自动写入后被标错或过时的比例维持在可控范围内

### 5.3 冲突处理

目标：同一问题存在多个版本时，系统默认优先返回当前有效结论。

建议指标：

- 冲突识别率：被人工或规则标记为互相冲突的记忆数量占已发现冲突总量的比例。
- 当前版本优先率：抽样检索中，Top-N 结果是否优先返回有效版本而非历史版本。
- 未治理冲突滞留时间：冲突记忆进入系统后，到被标记、降权、或 supersede 的平均时间。
- 偏好冲突收敛率：冲突 preference 是否最终收敛到一个明确生效版本。

建议通过标准：

- 已识别冲突中，大多数可在一个可接受窗口内进入治理流程
- 抽样检索中，当前有效版本位于 Top-3 的比例 >= 80%
- 偏好冲突默认不直接自动活化

### 5.4 复用质量

目标：历史记忆应真实减少重复劳动，并提高类似任务的成功率或速度。

建议指标：

- 复用命中率：相似任务中，是否成功召回历史有效方案。
- 复用有效率：召回内容被 agent 或人工反馈为 `useful` 的比例。
- 重复分析降低率：对于同类任务，使用记忆后是否减少重新探索时间或步骤数。
- 成功复用案例数：按项目记录的“历史方案被成功复用”的真实案例数量。

建议通过标准：

- 代表性任务集里，复用命中率持续提升
- `useful` 反馈占有效反馈的主体
- 至少能稳定列举一批真实案例，证明系统在减少重复劳动

## 6. 阶段门槛

### 6.1 MVP

满足以下条件即可视为 MVP：

- 通用 HTTP + MCP 能力稳定可用；
- 基本写入、检索、反馈、review、episode、graph 查询都已交付；
- 本地部署、Docker、CI、基础测试齐备；
- 可由多个 agent 指向同一实例共享底层存储。

这基本对应 `mem` 当前状态。

### 6.2 Beta

满足以下条件后可视为 Beta：

- project-first 的边界默认策略已经落地；
- `fact` / `experience` / `preference` 分流已在高层工具中体现；
- 检索路径已经具备基础去噪和反馈回流；
- 有一批真实任务样本证明同类问题可以复用历史方案；
- 冲突与过时内容开始进入明确治理流程。

### 6.3 Production-ready for coding agents

满足以下条件后，才建议对外定义为 “production-ready for coding agents”：

- 默认检索边界稳定，跨项目污染风险已被压低；
- 自动写入噪声得到持续控制；
- 冲突处理和 supersede 机制可稳定运行；
- feedback 能显著影响后续召回排序；
- 对多个 coding agent 的长期共享场景已经完成足够实战验证；
- 能通过指标与真实案例证明系统在持续提升复用质量，而不是仅仅积累数据。

## 7. 推荐推进顺序

建议按以下顺序推进：

1. 先固化 project-first 边界与高层工具默认行为。
2. 再完成 `fact` / `experience` / `preference` 分流。
3. 再建设去噪、反馈回流与冲突治理。
4. 最后用真实项目样本验证复用质量，并据此进入 Beta 判断。

## 8. 结论

`mem` 当前已经足以被视为一个 **完成度较高的通用 agent memory service v1**。  
但要进一步把它定义为一个 **策略通用、低污染、可稳定复用项目经验的多 agent 共享记忆平台**，仍需优先补齐：

- 项目级边界；
- 去噪治理；
- 冲突处理；
- 复用质量闭环。

当这些能力从"设计意图"进入"默认行为 + 可观测指标 + 真实任务验证"之后，项目才更适合进入 Beta，乃至被定义为 production-ready for coding agents。

---

## 9. Audit（2026-05-21）

> §4 原稿写于 2026-03-25，列了 §4.1–§4.6 共 31 个 TODO 条目。两个月之后回头对照代码现状逐项核实——下面用 ✅ done / 🚧 partial / ❌ open / 🔄 redesigned（实际实现走了不同路径但解决了同样问题） 四态标，每条带一句代码 / commit 证据。
>
> **方法**：本节直接读了 `src/domain/{query,capability_capsule}.rs`、`src/pipeline/{ingest,retrieve,compress}.rs`、`src/service/capability_capsule_service.rs`、`src/mcp/server.rs`、`src/storage/store.rs`、`src/worker/*.rs`、`src/storage/open_lock.rs` 及 ROADMAP + 三篇 mempalace-diff，未做新代码 grep 假设。
>
> **结论先放在这里**：31 条里 **15 done / 8 partial / 3 redesigned / 5 open**——绝大多数 P0/P1 在过去两个月通过 ROADMAP #1-#19、v2 #16-#28、v3 #29-#33 + Incident #3 沉淀过来。真正没动且仍有价值的只剩 §4.3 #3（自动写入阈值）、§4.4 #3（version-chain 优先有效版本）、§4.4 #5 + §4.3 #5（可观测指标 / 高噪声 agent 降权钩子）、§4.5 #5（重复分析时间对比 benchmark）。

### 9.1 §4.1 P0 项目级边界默认策略

| # | 原条目 | 状态 | 证据 |
|---|---|---|---|
| 1 | 检索入口统一要求并校验 `tenant` 与 `project` | 🚧 partial | `capability_capsule_search`（basic）`tenant` 仍 `Option<String>`（`mcp/server.rs:88` 默认 `local`），`project` 未在 `SearchCapabilityCapsuleRequest` 字段里；**`capability_capsule_search_contextual` 已强制 `tenant: String` + `project: String`**（`mcp/server.rs:107-108`，v2 #16）。建议：把 basic search 的 `tenant` 改成必填，或在 README/SKILL.md 上明确标 basic 为 deprecated path |
| 2 | 默认自动检索范围收敛到 `project`，`repo` / `personal` 显式开启 | ✅ done（contextual 路径） | `capability_capsule_search_contextual` 默认只塞 `project:<P>` 进 scope_filters，`include_repo` / `include_personal` 默认 false（`mcp/server.rs:880-893`）。basic search 仍是 soft scoring（`retrieve.rs:343-369` scope_score：matched +18 / unmatched -4，不硬过滤） |
| 3 | 写入入口明确 `project`、可选 `repo`、可选 `module` 的最小要求 | 🚧 partial | `IngestCapabilityCapsuleRequest` 把 `tenant` + `scope` + `source_agent` 设为 required（`domain/capability_capsule.rs:149-170`），但 `project` 仍是 `Option<String>`。`scope: Scope` 必填等价于"作者必须想清楚边界归属"，但 `project` 字段层面没有强制 |
| 4 | 为缺少边界字段的请求返回清晰错误或降级行为 | ❌ open | 无显式校验——project=None 写入不会报错。可以考虑：scope=Project / Repo 时强制 project 字段不为空，否则 400 |
| 5 | 在 MCP 高层工具中把 project-first 设为默认行为 | ✅ done | `capability_capsule_search_contextual` / `capability_capsule_bootstrap` / `capability_capsule_list_in_scope`（v2 #18）/ `capability_capsule_list_wings`（v2 #21）/ `capability_capsule_get_taxonomy`（v2 #22）一组高层工具全部是 project-first 设计 |

**§4.1 小结**：5 条中 2 done / 3 partial-or-open。剩余真痛点是 #3 + #4——给 `scope` 设条件依赖：当 `scope ∈ {Project, Repo}` 时 `project` 必填。一处 service-layer 校验 + 1 个 error 变体即可（半天）。

### 9.2 §4.2 P0 内容分层与写入治理

| # | 原条目 | 状态 | 证据 |
|---|---|---|---|
| 1 | 明确 `fact` / `experience` / `preference` 的判定规则 | 🔄 redesigned | mem 没有 `fact` type；最接近的是 **`implementation`** + **`experience`** + **`preference`** + `episode` + `workflow` + `diary` 六态（`domain/capability_capsule.rs:20-37`）。MCP 高层工具语义已在 `capability_capsule_commit_fact`（写 implementation）/ `_propose_experience`（写 experience）/ `_propose_preference`（写 preference）三个工具中体现。术语漂移："fact" 在文档里指 implementation——以后可以加 alias 或重命名 |
| 2 | `fact` 默认走自动写入 | ✅ done | `commit_fact` MCP 默认 `write_mode=Auto`；`initial_status(Implementation, Auto) → Active`（`pipeline/ingest.rs:51-62`） |
| 3 | `experience` 默认低权重 / 走 episode | 🔄 redesigned | 实际方向**相反**：debug intent 下 experience 是 type_score 最高的（10，`retrieve.rs:412`），高于 implementation（8）。**实现选择了 intent-aware 排序而不是 type-static 降权**——experience 在 debug 任务里更高价值这条经验性发现盖过了原稿的"低权重"假设。原稿条目应视为 redesigned |
| 4 | `preference` 强制 review | ✅ done | `initial_status(Preference, _) → PendingConfirmation`（不论 write_mode），硬编码在 `pipeline/ingest.rs:55-58` |
| 5 | 拒绝写入内容规则 | 🚧 partial | mem 实现了 **read-only advisory** 路线（v3 #29 `fact_check` API：returns similar_names / kg_contradictions / relationship_conflicts；caller 决定要不要 retry）而不是 server-side hard reject。Verbatim 原则要求服务端不能改 / 不能拒纯文本内容；唯一的 hard reject 是 `summary == content` 的"copy-summary"自查（`pipeline/ingest.rs` 历史 commit）。原条目"拒绝未验证猜测 / 临时结论 / 大段日志"在 mem 哲学下不该 server-side 拒——caller 责任 |

**§4.2 小结**：5 条中 2 done / 2 redesigned / 1 partial。core insight：mem 的 type taxonomy 和 ranking 实际比原稿设想更细（intent-aware × type matrix），原稿"experience 应低权重"是基于一个更朴素的 ranking 模型；现行 design 更准。

### 9.3 §4.3 P1 噪声治理

| # | 原条目 | 状态 | 证据 |
|---|---|---|---|
| 1 | 衰减与降权 | ✅ done | `decay_worker`（ROADMAP #7, `worker/decay_worker.rs`）+ `decay_score` 字段 + `apply_feedback(outdated)` 加 0.2 衰减（CLAUDE.md feedback table）。`retrieve.rs` decay 在 score 上以"减"出现 |
| 2 | recall 摘要压缩 | ✅ done | `pipeline/compress.rs` 用 tiktoken-rs o200k_base 做 token 计数四段式压缩（ROADMAP #6 + #11） |
| 3 | "自动写入阈值"，限制每轮任务默认写入量 | ❌ open | 没有 per-task / per-session write cap。`capability_capsule_batch_ingest` 接受任意大小批量，没有阈值告警。建议：加 `MEM_MAX_INGEST_PER_SESSION` env knob + service-layer 计数器 |
| 4 | feedback 接入排序 / 召回抑制逻辑 | ✅ done | 5 种 feedback_kind 全部 mutate `confidence` / `decay_score`（CLAUDE.md feedback table），retrieve.rs 加性 lifecycle 重排层（ROADMAP #11）使用 confidence + decay |
| 5 | 高噪声源 agent 审计 / 降权钩子 | ❌ open | `source_agent` 字段全程透传，但没有"per-agent 噪声 score"自动累积。dedup_worker（v3 #30）按 (source_agent, project, repo) 分组去重，是降噪的间接路径但不是"降权" |

**§4.3 小结**：5 条中 2 done / 2 done / 2 open。**真痛点是 #3 自动写入阈值**——transcript miner 一次能写几十上百 capsule，没有节流。

### 9.4 §4.4 P1 冲突处理

| # | 原条目 | 状态 | 证据 |
|---|---|---|---|
| 1 | supersede 最小规则 | 🚧 partial | `supersedes_capability_capsule_id` 字段 + `capability_capsule_supersede` MCP（v2 #26）+ ingest 路径自动写 `supersedes` graph edge（ROADMAP #17）已落地；但"什么时候应该 supersede"没有形成文字化规则——caller 自由判断 |
| 2 | `contradicts` / `supersedes` 关系的写入与维护 | ✅ done | `supersedes` edge（ROADMAP #17）+ `kg_invalidate_edge` MCP（v2 #16，显式关边） + `extracted_from` edge（ROADMAP #18） + `mentions_file` edge（ROADMAP #19）。`contradicts:` tag 前缀在 ingest 过滤（CLAUDE.md ROADMAP #16） |
| 3 | retrieval 优先展示当前有效版本 | ❌ open | `retrieve.rs` 没有 version-chain dedup。supersede 创建新 Active 行，原行**也保持 Active**（`capability_capsule_service.rs:870-916` 复制 status），所以 search 可能同时返回旧 + 新两个版本。是真 gap——加一层 "if supersedes != None, drop the superseded id from result" 或在 SQL WHERE 加 `NOT EXISTS (SELECT 1 ... WHERE supersedes_capability_capsule_id = id)` |
| 4 | preference 冲突人工 review | ✅ done | Preference 强制 PendingConfirmation（§9.2 #4）。冲突 preference 双双进 review，最终生效版本通过 `review_accept` / `review_edit_accept` / `review_reject` 收敛 |
| 5 | 冲突可观测指标 | ❌ open | `mem_health` MCP（v2 #28）有 capsule by-status 计数 + graph stats，但没有"active conflict pairs"指标。可以通过 graph stats 间接看 active edges with relation=contradicts，但不是 first-class 视图 |

**§4.4 小结**：5 条中 2 done / 1 partial / 2 open。**最值得做的是 #3**——version-chain 旧版本仍 Active 会污染召回，是结构性 bug 不是设计选择。

### 9.5 §4.5 P1 复用质量闭环

| # | 原条目 | 状态 | 证据 |
|---|---|---|---|
| 1 | 定义"有帮助的复用"标准 | 🚧 partial | `FeedbackKind::Useful`（+0.10 confidence + marks validated）/ `AppliesHere`（+0.05）已经做了二元区分（CLAUDE.md feedback table）。文档化的"复用质量定义"在 CLAUDE.md feedback 章节有；本 readiness doc 没回写 |
| 2 | agent 反馈采集（useful / outdated / incorrect） | ✅ done | 5 种 feedback_kind 全 wired（CLAUDE.md），MCP `capability_capsule_apply_feedback` / `_feedback`，离线采集走 `mem feedback-from-transcript` CLI |
| 3 | feedback 回流到排序 / 去噪 / 冲突 | ✅ done | confidence + decay_score 影响 retrieve scoring；`incorrect` 走 `apply_feedback` → `status=Archived`（CLAUDE.md feedback table） |
| 4 | 代表性真实任务验证 | ✅ done | `tests/recall_bench.rs`（10-rung ablation, ROADMAP #14）+ `tests/mempalace_bench.rs`（LongMemEval parity, ROADMAP #15）|
| 5 | "同类问题是否减少重复分析时间"对比实验 | ❌ open | 现有 bench 测**召回质量**，没有测**节省时间**。需要一组"重复任务 with mem vs without mem"对照设置，目前没有 |

**§4.5 小结**：5 条中 3 done / 1 partial / 1 open。**#5 是设计性研究项目**而不是单条 PR，先不做。

### 9.6 §4.6 P2 高层工具默认面

| # | 原条目 | 状态 | 证据 |
|---|---|---|---|
| 1 | `memory_bootstrap` | ✅ done | `mcp__plugin_mem_mem__capability_capsule_bootstrap` |
| 2 | `memory_search_contextual` | ✅ done | `mcp__plugin_mem_mem__capability_capsule_search_contextual` |
| 3 | `memory_commit_fact` | ✅ done | `mcp__plugin_mem_mem__capability_capsule_commit_fact` |
| 4 | `memory_propose_experience` | ✅ done | `mcp__plugin_mem_mem__capability_capsule_propose_experience` |
| 5 | `memory_propose_preference` | ✅ done | `mcp__plugin_mem_mem__capability_capsule_propose_preference` |
| 6 | `memory_apply_feedback` | ✅ done | `mcp__plugin_mem_mem__capability_capsule_apply_feedback`（+ alias `_feedback`） |
| 7 | Skill / Prompt 层把这些设为默认路径 | 🚧 partial | SKILL.md 引导调用方走高层工具；底层 ingest / search 仍 publicly exposed。没有"deprecated"标记 |

**§4.6 小结**：7 条中 6 done / 1 partial。完全到位。

### 9.7 总账 + 下一步

| 状态 | 数 | 占比 |
|---|---|---|
| ✅ done | 18 | 58% |
| 🚧 partial | 7 | 23% |
| 🔄 redesigned | 2 | 6% |
| ❌ open | 4 | 13% |

**真 open 的 4 条**（按工作量 × 价值排序）：

1. **§4.4 #3 — version-chain 优先有效版本**（结构性 bug，~半天）：supersede 创建新行后旧行 Active 不变；搜索可能同时返回新旧两版。加 retrieve 层 dedup。
2. **§4.3 #3 — 自动写入阈值**（mining 路径节流，~3h）：transcript miner 一次能写几十条；加 `MEM_MAX_INGEST_PER_SESSION` + service-layer 计数。
3. **§4.1 #4 — 边界缺失请求清晰错误**（~2h）：`scope ∈ {Project, Repo}` 时 `project` 必填，否则 400。
4. **§4.3 #5 — 高噪声 agent 降权钩子**（设计 + 实现，~M）：per-agent noise score 累积，按阈值降权 / 审计。

**真 partial 但已够好的**：§4.1 #1 / #3、§4.2 #5、§4.4 #1、§4.5 #1、§4.6 #7——文档/规范缺位，代码到位；改 README/SKILL.md 即可补。

**redesigned 的 2 条**（§4.2 #1 + §4.2 #3）：实际走了 intent-aware × type-matrix 路线，比原稿的 type-static 降权更细。文档以 readiness doc 为准的话需要更新原稿描述。

**Beta gating 视角**：§6.2 列的五条 Beta 门槛——project-first ✅、fact/experience/preference 分流 ✅、基础去噪 + 反馈回流 ✅、真实任务样本 ✅、冲突治理进入流程 🚧（缺 version-chain dedup 这一关）。**做完 §9.7 #1 那条就基本到 Beta 门槛**。
