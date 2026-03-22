# Codex / 多进程 Agent 接入 mem：MCP + Skill 设计

日期：2026-03-21  
状态：Draft（已与人对齐方向，待实现与评审）

## 1. 目标

在 **多种 Codex / Agent 运行形态混合**（本机 CLI、IDE 内会话、CI/无头等）的前提下，让多个进程 **共享同一份记忆**，减少重复推理与 token，并尽量 **智能化**（可重复的读/写策略，而非再堆叠一个独立「大脑」服务）。

## 2. 结论摘要（推荐架构）

采用 **双轨**：

| 轨道 | 适用场景 | 作用 |
|------|----------|------|
| **MCP + Skill** | Cursor、支持 MCP 的 Codex、交互式会话 | MCP 暴露对 `mem` HTTP API 的封装工具；Skill 描述 **何时、如何用** 这些工具（流程、默认值、`caller_agent` 等）。 |
| **HTTP（或薄 CLI）** | CI、无头任务、无 MCP 宿主 | 直接用 `curl`/脚本调用与 README 一致的 REST 接口；与 MCP **共用** `MEM_BASE_URL`、`tenant`、`scope` 约定。 |

**智能化** 主要落在 **Skill（+ 可选 AGENTS.md）** 的调用策略与 **MCP 工具描述** 的准确性上；`mem` 侧继续负责检索、压缩包、生命周期与混合检索（已实现部分）。

## 3. 共享模型

- **单一事实来源**：一个长期运行的 `mem` 进程 + **一份 DuckDB**（通过 `MEM_DB_PATH` 指向共享路径；单机多终端/多进程足够）。
- **客户端不直接多写 DuckDB**：仅通过 HTTP（或经 MCP 转 HTTP）访问，避免多进程直接挂载 DB 文件写入。
- **隔离**：`tenant` 区分团队/项目空间；`scope_filters`（如 `repo:`、`module:`）限制检索范围，降低跨仓库污染。
- **溯源**：统一使用 `caller_agent` 区分来源，例如 `codex-cli`、`cursor`、`ci:<job_id>`，便于统计与后续按来源策略扩展。

## 4. MCP 设计要点

MCP server 为 **薄适配层**（可独立仓库或本 monorepo 子包），职责：

- 读取环境变量：`MEM_BASE_URL`（默认 `http://127.0.0.1:3000`）、`MEM_TENANT`（默认 `local`）等。
- 将下列能力映射为 **工具**（名称与字段在实现阶段与 OpenAPI/示例 JSON 对齐）：

| 工具（建议名） | 对应 mem API | 说明 |
|----------------|--------------|------|
| `memory_search` | `POST /memories/search` | 核心读路径；必填 `query`，可选 `intent`、`scope_filters`、`token_budget`、`expand_graph`、`caller_agent`。 |
| `memory_ingest` | `POST /memories` | 写入候选记忆；遵守 `write_mode` 与 `memory_type` 生命周期。 |
| `memory_get` | `GET /memories/{id}` | 详情 + 版本链 + 图边 + embedding 元数据等。 |
| `memory_feedback` | `POST /memories/feedback` | `useful` / `outdated` 等，反哺排序。 |
| `memory_list_pending_review` | `GET /reviews/pending` | 可选，供「人工审核」类工作流。 |
| `episode_ingest` | `POST /episodes` | 可选，用于沉淀成功运行片段与工作流候选。 |
| `embeddings_*` | `GET/POST /embeddings/*` | 可选，仅维护/调试角色暴露，默认可对普通 Codex 隐藏。 |

错误处理：将 HTTP 4xx/5xx 与 body 简要透传为 MCP 工具错误信息，便于模型重试或换策略。

## 5. Skill 设计要点

Skill 文件（如 `SKILL.md`）**不重复** MCP 的 JSON 字段细节，而固定以下 **策略段落**：

1. **任务开始前**：在上下文足够时调用 `memory_search`；`token_budget` 保守；优先带 `repo:` / `module:` 等与当前仓库一致的 `scope_filters`。
2. **任务进行中**：遇到与历史决策/实现强相关的问题时，可再次 `memory_search` 或 `memory_get`。
3. **写入策略**：实现类事实可 `write_mode: auto`；偏好与强约束倾向 **pending / 需人审** 路径（与现有 `mem` 模型一致）。
4. **任务结束后（可选）**：对可复用的成功路径调用 `episode_ingest`；或对高价值结论调用 `memory_ingest`。
5. **`caller_agent`**：必须设为可区分当前运行环境（CLI / IDE / CI）的字符串。

说明：Skill **不能强制执行**；重要习惯可同时在项目 `AGENTS.md` 或用户规则中重申。

## 6. CI / 无头回退

- 无 MCP、无 Skill 解析时：使用 **HTTP 脚本**（Makefile、GitHub Actions step 等），在 job 开始 `memory_search`、结束 `episode_ingest` 或条件写入。
- 与 MCP **同一** `MEM_BASE_URL` 与 `tenant`，保证与交互式会话看到同一记忆库。

## 7. 安全与非目标（本期）

- **本期假设**：`mem` 监听本机或可信内网；不对公网暴露。
- **后续**：若远程 Codex 访问集中式 `mem`，需 **TLS + 认证**（API key 或 mTLS），单独开 spec。
- **非目标**：分布式多写 DuckDB、多租户 SaaS 级隔离（与主记忆服务设计一致）。

## 8. 与现有文档的关系

- HTTP 契约与示例以仓库根目录 `README.md` 为准；本 spec 只定义 **接入形态与责任边界**。
- 记忆域模型与检索管线见 `docs/superpowers/specs/2026-03-21-ai-agent-memory-service-design_zh.md` 及混合检索相关 plan。

## 9. 实现顺序建议（供后续 plan 使用）

1. 实现最小 MCP server：`memory_search` + `memory_ingest` + `memory_get`。  
2. 编写 Skill（流程 + 环境变量说明）。  
3. 增加 CI 示例片段（HTTP）。  
4. 按需扩展工具：`feedback`、`episodes`、pending review。  
5. 文档：`README` 增加「Codex / MCP」小节链接本 spec。

---

**评审**：实现前请对照本 spec 核对 MCP 工具命名与 `mem` API 是否一一对应；Skill 正文在实现仓库或本仓库 `docs` 中版本化管理。
