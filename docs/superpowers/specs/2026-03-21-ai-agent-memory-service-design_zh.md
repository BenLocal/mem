# AI Agent 记忆服务设计

日期：2026-03-21
状态：Draft

## 1. 概述

这个项目定义了一个面向个人、本地优先的记忆服务，供多个 AI agent 在代码工程任务中共享使用。这个服务的目标，是通过检索压缩后、可复用的记忆，而不是反复回放大量历史上下文，来提升交付速度、减少重复推理，并降低 token 消耗。

系统主要针对以下目标进行优化：

- 复用业务实现知识
- 复用调试经验和执行经验
- 持久化用户偏好和长期约束
- 将成功流程沉淀为可复用的任务模式

存储模型采用 DuckDB 作为面向事实和检索的主记忆存储，采用 IndraDB 作为关系与推理图谱。

## 2. 目标

### 核心目标

- 构建一个可供多个 agent 共享使用的记忆服务
- 存储来自真实工程工作的实现知识
- 存储能够加速后续工作的实践经验
- 在需要人工确认的前提下存储用户偏好
- 总结成功工作流，供后续复用
- 通过返回任务相关的压缩记忆包来降低 token 使用量

### v1 非目标

- 分布式部署
- 跨用户的多租户 SaaS 功能
- 对所有自动生成记忆都完全自动信任
- 除基础审核和维护界面外的复杂 UI
- 大规模事件溯源基础设施

## 3. 高层架构

服务由五个主要模块组成。

### 3.1 Ingest API

接收来自 agent 和维护工具的写入请求。它负责接收候选记忆、审核动作、反馈和图查询。

### 3.2 Memory Pipeline

负责对候选记忆执行以下处理：

- 记忆分类
- 摘要与标准化
- 去重与版本管理
- 重要性与置信度打分
- 确认路由
- 实体与关系抽取
- 从成功 episode 中抽取工作流

### 3.3 DuckDB Memory Store

DuckDB 是记忆记录和检索相关数据的事实来源。它存储：

- 规范化后的记忆条目
- embedding 和检索元数据
- episode 与写入事件
- 验证与反馈记录
- 版本链与生命周期状态
- 代码引用和来源证据

### 3.4 IndraDB Knowledge Graph

IndraDB 存储用于扩展和重排的高价值实体与关系。它不是原始记忆文本的事实来源。它存储：

- 项目、仓库、模块、服务
- 任务模式、问题模式、决策
- 用户偏好与约束
- 记忆到实体的链接
- 冲突与替代关系边

### 3.5 Retrieval Orchestrator

负责搜索与上下文组装。它从 DuckDB 检索候选记忆，利用图谱扩展相关上下文，对结果进行重排，并返回适配调用方 token 预算的压缩记忆包。

## 4. 记忆模型

所有记忆类型共享以下公共元数据：

- `memory_id`
- `memory_type`
- `status`
- `scope`
- `version`
- `confidence`
- `source_agent`
- `created_at`
- `updated_at`
- `last_validated_at`
- `decay_score`
- `content_hash`
- `supersedes_memory_id`

规划阶段建议明确以下生命周期字段：

- `status`：`pending_confirmation`、`provisional`、`active`、`archived`、`rejected`
- `confidence`：归一化分数，范围为 `[0.0, 1.0]`
- `decay_score`：归一化分数，范围为 `[0.0, 1.0]`，数值越高表示默认越不应被召回

### 4.1 Implementation Memory

用于存储业务实现知识和具体工程事实，例如：

- 某个功能是如何实现的
- 某模块的约束条件
- 集成细节
- 带证据的重复 bug 修复模式

主要载荷字段：

- `summary`
- `evidence`
- `code_refs`
- `project`
- `repo`
- `module`
- `task_type`
- `tags`

写入模式：默认自动写入。

### 4.2 Experience Memory

用于存储实践中的执行知识，例如：

- 更有效的调试顺序
- 常见操作捷径
- 仓库特定的工作习惯
- 坑点和启发式经验

写入模式：默认自动写入，但相比 implementation memory 需要更强的衰减和去重机制。

### 4.3 Preference Memory

用于存储用户偏好和长期约束，例如：

- 沟通偏好
- 偏好的实现风格
- 重构边界
- 仓库级或项目级约束

写入模式：只能提议写入。激活前必须经过人工确认。

### 4.4 Episode Memory

用于存储工作过程摘要，包括决策、尝试、结果和失败。Episode memory 作为原材料，用于蒸馏出更稳定的记忆。

写入模式：自动写入。

### 4.5 Workflow Memory

用于存储可复用的成功流程，面向重复出现的工程任务。这是一个一级记忆类型，因为系统的目标之一不仅是捕获事实和经验，还要捕获可复用的成功工作过程。

示例用途：

- 某业务域的功能交付流程
- 某服务域的典型调试流程
- 某仓库中安全的修改与验证顺序
- 某类重复故障的调查流程

主要载荷字段：

- `goal`
- `preconditions`
- `steps`
- `decision_points`
- `success_signals`
- `failure_signals`
- `evidence`
- `scope`

写入模式：由系统生成候选项，人工确认后激活。

## 5. 知识图谱模型

图谱应保持克制。v1 只应建模高价值节点和关系。

### 5.1 节点类型

- `Project`
- `Repo`
- `Module`
- `Service`
- `TaskPattern`
- `IssuePattern`
- `Decision`
- `UserPreference`
- `Workflow`
- `Memory`

### 5.2 关系类型

- `applies_to`
- `depends_on`
- `observed_in`
- `fixed_by`
- `derived_from`
- `prefers`
- `contradicts`
- `supersedes`
- `relevant_to`
- `uses_workflow`

图谱的作用，是解释为什么某条记忆在当前上下文中相关，而不是尝试表示所有低价值对象。

## 6. 写入路径

v1 使用两条写入通道。

### 6.1 自动写入通道

适用于：

- Implementation memory
- Experience memory
- Episode memory

处理流程：

1. agent 提交候选记忆
2. pipeline 对其进行分类和标准化
3. 系统执行去重和冲突检查
4. 系统分配置信度和生命周期状态
5. 记忆写入 DuckDB
6. 相关实体和关系写入 IndraDB

当质量或证据较弱时，新的自动记忆可以先以 `provisional` 状态进入系统。

建议的状态流转：

- 自动写入：`provisional -> active -> archived`
- 确认写入：`pending_confirmation -> active | rejected`
- 冲突或过时不要求硬删除；相关记忆可以保持 `active` 但降低排序，也可以在失效后进入 `archived`

建议的分数字段语义：

- `confidence` 会随着重复成功复用、正向反馈和更强证据而上升
- `confidence` 会随着显式负反馈、被更新证据反驳或来源质量较弱而下降
- `decay_score` 会随着陈旧、低复用或关联代码发生重大变化而上升
- 检索时应优先考虑更高 `confidence` 和更低 `decay_score`

### 6.2 确认写入通道

适用于：

- Preference memory
- Workflow memory
- 高影响的规则类知识

处理流程：

1. agent 或 pipeline 提议候选项
2. 候选项进入 `pending_confirmation`
3. 人工接受、拒绝或编辑
4. 被接受的候选项变为 `active`

## 7. 冲突、衰减与可信度

系统必须优先保证正确性，而不是盲目追求召回量。

### 7.1 重复处理

当新记忆与已有记忆高度相似时：

- 优先合并或升级版本，而不是盲目新增
- 保留来源证据
- 追踪替代链

### 7.2 冲突处理

当两条记忆彼此矛盾时：

- 保留两条记录
- 增加 `contradicts` 关系
- 检索时优先使用更新、更高验证度、作用域更匹配的记忆
- 如果歧义仍然重要，应在压缩输出中显式标记冲突

### 7.3 衰减与归档

当记忆满足以下条件时，应逐步降权：

- 很少被复用
- 多次被标记为无帮助
- 与已发生大改的代码区域绑定
- 很旧且从未重新验证

### 7.4 证据要求

Implementation memory 应尽量附带证据链：

- 代码引用
- 任务摘要
- 验证结果
- 错误现象
- 来源 agent 身份

没有证据的记忆也可以存储，但默认可信度应更低。

## 8. 检索与上下文压缩

检索链路应优化“每个 token 的有效性”，而不是原始召回量。

### 8.1 查询理解

每个请求都应先被分类为某种任务意图，例如：

- 功能实现
- 代码理解
- 问题排查
- 偏好查询
- 工作流复用

### 8.2 从 DuckDB 召回候选

候选记忆可通过以下组合方式召回：

- 文本匹配
- embedding 相似度
- scope 过滤
- 项目或仓库过滤
- 模块与任务类型过滤
- 历史成功复用信号

### 8.3 图谱扩展与重排

候选记忆会映射到图谱实体，并沿着有限步相关关系进行扩展。重排时应考虑：

- 语义相似度
- scope 匹配度
- 记忆类型权重
- `confidence`
- 验证次数
- 新鲜度
- 证据强度
- 与用户偏好的兼容性

### 8.4 上下文压缩输出

响应应返回一个紧凑的记忆包，包含四个部分。

#### Directives

短小、高优先级的约束与偏好，agent 当前必须遵守。

#### Relevant Facts

当前任务直接需要的事实、边界、依赖和历史实现细节。

#### Reusable Patterns

调试启发、工程捷径和可复用的经验级指导。

#### Suggested Workflow

当系统识别出当前任务存在可复用工作流模式时，返回一个压缩后的步骤化成功流程。

### 8.5 基于预算的输出

orchestrator 必须支持显式 token 预算。当预算紧张时：

- 优先保留 directives
- 只保留最相关的 facts
- 对 patterns 做更激进的压缩
- workflow 只返回提纲，而不是完整细节

完整证据只在按需展开时返回。

## 9. API 边界

初始 API 应保持小而明确。

### 9.1 `ingest_memory`

将候选记忆写入系统。

代表性字段：

- `memory_type`
- `content`
- `scope`
- `source_agent`
- `project`
- `repo`
- `module`
- `code_refs`
- `evidence`
- `write_mode`
- `idempotency_key`

### 9.2 `search_memory`

针对任务返回压缩记忆包。

代表性字段：

- `query`
- `intent`
- `scope_filters`
- `token_budget`
- `caller_agent`
- `expand_graph`

### 9.3 `get_memory`

返回完整记忆记录，包括证据、版本链和图谱链接。

### 9.4 `feedback_memory`

接受如下反馈：

- 有用
- 已过时
- 不正确
- 适用于当前场景
- 不适用于当前场景

这些反馈会影响排序、可信度和衰减。

### 9.5 `review_pending_memories`

列出待确认条目，并支持：

- 接受
- 拒绝
- 编辑后接受

### 9.6 `graph_neighbors`

为高级调用方和维护工具返回局部图谱上下文。

## 10. 多 Agent 共享模型

这个服务从一开始就面向多个 agent。v1 应支持以下隔离和路由维度：

- `tenant`
- `scope`
- `visibility`
- `source_agent`

建议的语义：

- `tenant`：预留的顶层隔离键；v1 可以默认只有一个本地 tenant，但这个字段应存在，以避免未来 schema 大改
- `scope`：记忆的适用边界；建议取值为 `global`、`project`、`repo` 和 `workspace`
- `visibility`：可读性规则；建议取值为 `private`、`shared` 和 `system`

推荐的 scope 层级：

- `global`
- `project`
- `repo`
- `workspace`

为减少多个 agent 重复写入，系统应通过显式 key 或稳定内容哈希支持幂等。

## 11. 错误处理

v1 应显式处理以下失败模式。

### 11.1 低质量写入

示例：

- 摘要质量差
- 证据薄弱
- 抽取失败
- 接近重复的噪声写入

预期动作：

- 拒绝，或降级为 `provisional`

### 11.2 冲突召回

示例：

- 多条记忆给出了互不兼容的建议

预期动作：

- 保守重排
- 若无法消解，则在输出中标记冲突

### 11.3 作用域污染

示例：

- 来自其他仓库、看似相似但实际无关的记忆被召回

预期动作：

- 更强地依赖 scope 过滤
- 对弱 scope 的跨项目召回进行降权

### 11.4 记忆过时

示例：

- 代码结构已经变化
- 用户偏好已经变化
- 旧工作流已不再适用

预期动作：

- 重新验证、衰减或归档

## 12. 评估指标

系统应按工程实用性来评估，而不是按存储量来评估。

建议的 v1 指标：

- `Precision@K`
- `Compressed Context Usefulness`
- `Token Savings`
- `Task Acceleration`
- `Memory Reuse Rate`
- `Bad Recall Rate`
- `Confirmation Burden`
- `Workflow Reuse Rate`

规划阶段建议的最低 v1 验收目标：

- `Precision@K`：在基准任务中，top-5 结果应在多数案例下包含有用记忆
- `Token Savings`：压缩输出相比直接回放原始任务历史，应显著降低上下文大小
- `Bad Recall Rate`：明显错误或过时的召回，在基准任务中应保持低频
- `Confirmation Burden`：待审核量应保持在人可以日常处理的范围内
- `Workflow Reuse Rate`：对重复性基准任务，系统应至少在部分场景中返回可用的工作流提纲，而不是只有零散事实

## 13. v1 测试策略

第一批验证应聚焦真实工程场景。

### 13.1 业务实现复用

给定一个与历史任务相似的新任务，验证系统是否能召回实现模式、约束条件和带代码引用的相关知识。

### 13.2 调试复用

给定一个重复出现的故障模式，验证系统是否能召回之前成功的排查路径和修复动作。

### 13.3 偏好遵循

给定一个应触发用户偏好的任务，验证系统是否会把这些约束正确注入压缩记忆包。

### 13.4 工作流复用

给定一个重复出现的工程任务类别，验证系统是否会返回成功且可复用的工作流提纲，而不只是若干孤立事实。

## 14. 推荐的 v1 方向

推荐实现方向如下：

- 使用 DuckDB 作为规范记忆和检索主存储
- 使用 IndraDB 作为选择性的语义与关系层
- 优先实现智能检索和图谱辅助重排，而不是追求最小化能力
- 对 preference memory 和 workflow memory 保留确认门槛
- 将 workflow 抽取视为一级目标

这样，系统的角色就很明确：它不只是一个记忆存储，而是一个面向多个 AI agent 的共享工程记忆与工作流复用服务。
