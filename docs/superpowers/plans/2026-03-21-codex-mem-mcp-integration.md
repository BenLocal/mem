# Codex / MCP 接入 mem 实现计划

> **Status: COMPLETE**（实现与 CI 已落地；以下为原始任务分解，供追溯。）

> **For agentic workers:** REQUIRED SUB-SKILL: Use @superpowers/subagent-driven-development (recommended) or @superpowers/executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 交付一个可通过 **MCP** 调用本仓库 `mem` HTTP API 的薄适配服务，并配套 **Skill 文档** 与 **CI HTTP 示例**，使多进程 / 多环境（CLI、IDE、无头）共享同一 `MEM_BASE_URL` + `tenant` 下的记忆。

**Architecture:** 在仓库内新增独立 **Node + TypeScript** 包（不混入 Rust crate），使用官方 MCP SDK 注册工具；每个工具用 `fetch` 转发到 `mem` 的 REST 端点，从环境变量读取 `MEM_BASE_URL`、`MEM_TENANT`；将 HTTP 错误体转为 MCP 工具错误文本。Skill 仅描述策略与默认值，不重复 JSON schema。CI 示例用 `curl` 调用相同 API。

**Tech Stack:** Node.js ≥20、TypeScript、`@modelcontextprotocol/sdk`、`fetch`（Node 内置）、`vitest` + mock `fetch` 做单元测试。

**Spec:** `docs/superpowers/specs/2026-03-21-codex-mem-mcp-integration-design.md`

**交付物相对初版计划的扩展：** `mem_health`（纯文本 `/health`）、`memory_graph_neighbors`、`memory_review_{accept,reject,edit_accept}`；`mem-client` 含 `buildMemUrl`、`memRequestText`；仓库 `.github/workflows/ci.yml` 并行跑 Rust 与 mem-mcp。

---

## File map（创建 / 修改）

| 区域 | 路径 | 职责 |
|------|------|------|
| MCP 包 | `integrations/mem-mcp/package.json` | 脚本：`build`、`start`、`test` |
| MCP 包 | `integrations/mem-mcp/tsconfig.json` | `NodeNext` / `dist` |
| MCP 包 | `integrations/mem-mcp/src/config.ts` | `MEM_BASE_URL`、`MEM_TENANT`、`MEM_MCP_EXPOSE_EMBEDDINGS` |
| MCP 包 | `integrations/mem-mcp/src/mem-client.ts` | `buildMemUrl`、`memRequestJson`、`memRequestText` |
| MCP 包 | `integrations/mem-mcp/src/mem-client.test.ts` | fetch mock；含 `memories/search` body 形状断言 |
| MCP 包 | `integrations/mem-mcp/src/tool-result.ts` | `okJson` / `errResult` |
| MCP 包 | `integrations/mem-mcp/src/schemas.ts` | 共享 Zod 枚举 |
| MCP 包 | `integrations/mem-mcp/src/register-tools.ts` | 聚合注册 |
| MCP 包 | `integrations/mem-mcp/src/tools/*.ts` | 各工具模块（含 `health.ts`、`graph.ts`、`review-actions.ts`） |
| MCP 包 | `integrations/mem-mcp/src/index.ts` | stdio + `McpServer` |
| MCP 包 | `integrations/mem-mcp/README.md` | 安装、环境、`mcp.json`、工具表 |
| Skill | `docs/superpowers/skills/mem-mcp-codex/SKILL.md` | 流程、环境表、根 README 链接 |
| 根文档 | `README.md` | 「Codex / MCP」小节 |
| CI 示例 | `docs/superpowers/examples/ci-mem-http-snippet.md` | `curl` search / episode |
| CI | `.github/workflows/ci.yml` | `cargo fmt/clippy/test` + mem-mcp `npm ci/test/build` |

---

### Task 1: 脚手架 — `integrations/mem-mcp` 包

- [x] **Step 1–5:** `package.json`、`tsconfig`、`.gitignore`、`README`、依赖安装与构建通过。

---

### Task 2: HTTP 客户端 — `mem-client.ts`

- [x] **Step 1–5:** `memRequestJson`、错误信息、`mem-client.test.ts` 基础用例；后续迭代增加 `buildMemUrl`、`memRequestText`、`mem_health` 支持。

---

### Task 3: MCP 入口 + `memory_search` 工具

- [x] **Step 1–6:** `index.ts`、`tools/search.ts`；测试含 **`POST /memories/search` body**（`query`、`caller_agent` 等 snake_case）断言。

---

### Task 4: `memory_ingest` 与 `memory_get`

- [x] **Step 1–4:** `tools/ingest.ts`、`tools/memory-get.ts`（计划中的 `get.ts` 命名为 `memory-get.ts` 以避免与 Node 内置混淆）。

---

### Task 5: 扩展工具 — feedback、pending、episode

- [x] **Step 1–5:** `feedback.ts`、`pending.ts`、`episode.ts`。

---

### Task 6（可选）: Embeddings 维护工具

- [x] **Step 1–3:** `tools/embeddings.ts`；`MEM_MCP_EXPOSE_EMBEDDINGS=1` 时注册；README 已说明。

---

### Task 7: Skill — `docs/superpowers/skills/mem-mcp-codex/SKILL.md`

- [x] **Step 1–4:** 策略段落、**环境变量表**、链接 `integrations/mem-mcp/README.md`、根 `README.md`、spec、**本 plan**。

---

### Task 8: 根 README + CI HTTP 示例

- [x] **Step 1–3:** 根 `README`「Codex / MCP」；`docs/superpowers/examples/ci-mem-http-snippet.md`。

---

### Task 9（可选）: 仓库 CI 跑 mem-mcp 测试

- [x] **Step 1–2:** `.github/workflows/ci.yml` 含 Rust 与 mem-mcp 并行 job。

---

## Plan review

- 工具参数与 mem HTTP JSON 对齐；`tenant` 默认由 `MEM_TENANT` 注入；HTTP 错误体截断在 `mem-client`（≤2000 字符）。

---

## Execution handoff

**本计划已全部完成。** 后续增强（认证、远程 `mem`、更多工具的契约测试）另开 spec / plan。
