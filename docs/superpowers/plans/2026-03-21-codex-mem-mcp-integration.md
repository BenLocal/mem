# Codex / MCP 接入 mem 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use @superpowers/subagent-driven-development (recommended) or @superpowers/executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 交付一个可通过 **MCP** 调用本仓库 `mem` HTTP API 的薄适配服务，并配套 **Skill 文档** 与 **CI HTTP 示例**，使多进程 / 多环境（CLI、IDE、无头）共享同一 `MEM_BASE_URL` + `tenant` 下的记忆。

**Architecture:** 在仓库内新增独立 **Node + TypeScript** 包（不混入 Rust crate），使用官方 MCP SDK 注册工具；每个工具用 `fetch` 转发到 `mem` 的 REST 端点，从环境变量读取 `MEM_BASE_URL`、`MEM_TENANT`；将 HTTP 错误体转为 MCP 工具错误文本。Skill 仅描述策略与默认值，不重复 JSON schema。CI 示例用 `curl` 调用相同 API。

**Tech Stack:** Node.js ≥20、TypeScript、`@modelcontextprotocol/sdk`、`fetch`（Node 内置）、`vitest` 或 `node:test` + mock `fetch` 做单元测试。

**Spec:** `docs/superpowers/specs/2026-03-21-codex-mem-mcp-integration-design.md`

---

## File map（创建 / 修改）

| 区域 | 路径 | 职责 |
|------|------|------|
| MCP 包 | `integrations/mem-mcp/package.json` | 脚本：`build`、`start`（`node dist/index.js`）、`test` |
| MCP 包 | `integrations/mem-mcp/tsconfig.json` | `module`/`target` 与 `outDir: dist` |
| MCP 包 | `integrations/mem-mcp/src/config.ts` | 读取 `MEM_BASE_URL`、`MEM_TENANT`、`MEM_MCP_EXPOSE_EMBEDDINGS`（可选） |
| MCP 包 | `integrations/mem-mcp/src/mem-client.ts` | `request(method, path, body?)`，统一 base URL、JSON、`Content-Type`、错误格式化 |
| MCP 包 | `integrations/mem-mcp/src/tools/search.ts` | `memory_search` → `POST /memories/search` |
| MCP 包 | `integrations/mem-mcp/src/tools/ingest.ts` | `memory_ingest` → `POST /memories` |
| MCP 包 | `integrations/mem-mcp/src/tools/get.ts` | `memory_get` → `GET /memories/:id?tenant=` |
| MCP 包 | `integrations/mem-mcp/src/tools/feedback.ts` | `memory_feedback` → `POST /memories/feedback` |
| MCP 包 | `integrations/mem-mcp/src/tools/pending.ts` | `memory_list_pending_review` → `GET /reviews/pending?tenant=` |
| MCP 包 | `integrations/mem-mcp/src/tools/episode.ts` | `episode_ingest` → `POST /episodes` |
| MCP 包 | `integrations/mem-mcp/src/tools/embeddings.ts`（可选） | 仅当 `MEM_MCP_EXPOSE_EMBEDDINGS=1` 时注册 `embeddings_list_jobs`、`embeddings_rebuild`、`embeddings_providers` |
| MCP 包 | `integrations/mem-mcp/src/index.ts` | `Server` stdio transport，注册全部工具 |
| MCP 包 | `integrations/mem-mcp/README.md` | 安装、环境变量、Cursor `mcp.json` 片段、本地联调步骤 |
| MCP 测试 | `integrations/mem-mcp/src/mem-client.test.ts`（或 `tests/`） | mock `globalThis.fetch`，断言 URL 与 body |
| Skill | `docs/superpowers/skills/mem-mcp-codex/SKILL.md` | 流程段落 + 指向 MCP 工具名 + 环境变量表 |
| 根文档 | `README.md` | 新增小节「Codex / MCP」：链接 spec、plan、`integrations/mem-mcp`、Skill 路径 |
| CI 示例 | `docs/superpowers/examples/ci-mem-http-snippet.md` | `curl` 调用 `search` + 可选 `episode` 的可复制块 |

**说明：** 若你更倾向 **Python + mcp**，可将 `integrations/mem-mcp/` 换为 `integrations/mem_mcp_py/`，任务拆分等价，本计划以 TS 为默认以降低与 MCP 官方示例的摩擦。

---

### Task 1: 脚手架 — `integrations/mem-mcp` 包

**Files:**
- Create: `integrations/mem-mcp/package.json`
- Create: `integrations/mem-mcp/tsconfig.json`
- Create: `integrations/mem-mcp/.gitignore`（`node_modules/`, `dist/`）
- Create: `integrations/mem-mcp/README.md`（stub）
- Modify: 无

- [ ] **Step 1:** 在 `integrations/mem-mcp` 执行 `npm init -y`，添加依赖 `@modelcontextprotocol/sdk`、`typescript`、`@types/node`，`devDependencies` 选 `vitest` 或 `tsx`。

- [ ] **Step 2:** `tsconfig.json`：`strict` true，`outDir`/`rootDir`，`moduleResolution` `node16` 或 `bundler`（与 SDK 类型一致即可）。

- [ ] **Step 3:** `package.json` scripts 示例：

```json
{
  "scripts": {
    "build": "tsc",
    "start": "node dist/index.js",
    "test": "vitest run"
  }
}
```

- [ ] **Step 4:** 运行 `npm install && npm run build`（空 `src/index.ts` 可先导出占位）**Expected:** 无报错。

- [ ] **Step 5:** Commit：`git add integrations/mem-mcp && git commit -m "chore(integrations): scaffold mem-mcp TypeScript package"`

---

### Task 2: HTTP 客户端 — `mem-client.ts`

**Files:**
- Create: `integrations/mem-mcp/src/config.ts`
- Create: `integrations/mem-mcp/src/mem-client.ts`

- [ ] **Step 1: 写失败测试** — `mem-client.test.ts`：mock `fetch` 返回 `ok: false`, `status: 503`, `text: "down"`，期望 `request` 抛出或返回 `Err` 且消息含 `503` 与 `down`。

- [ ] **Step 2:** `npm run test` **Expected:** FAIL（未实现）

- [ ] **Step 3: 最小实现** — `getConfig()`：`MEM_BASE_URL` 默认 `http://127.0.0.1:3000`（无尾斜杠）、`MEM_TENANT` 默认 `local`。`memRequest(method, path, { query?, body? })`：拼 URL，`POST` 带 `JSON.stringify(body)`，`Accept: application/json`；非 2xx 时 `throw new Error(\`mem HTTP ${status}: ${text}\`)`。

```typescript
// mem-client.ts（节选）
export async function memRequest(
  baseUrl: string,
  method: string,
  path: string,
  init?: { query?: Record<string, string>; body?: unknown }
): Promise<unknown> {
  const url = new URL(path.replace(/^\//, ""), baseUrl.endsWith("/") ? baseUrl : baseUrl + "/");
  if (init?.query) {
    for (const [k, v] of Object.entries(init.query)) url.searchParams.set(k, v);
  }
  const headers: Record<string, string> = { Accept: "application/json" };
  let body: string | undefined;
  if (init?.body !== undefined) {
    headers["Content-Type"] = "application/json";
    body = JSON.stringify(init.body);
  }
  const res = await fetch(url, { method, headers, body });
  const text = await res.text();
  if (!res.ok) throw new Error(`mem HTTP ${res.status}: ${text.slice(0, 2000)}`);
  return text ? JSON.parse(text) : null;
}
```

- [ ] **Step 4:** `npm run test` **Expected:** PASS

- [ ] **Step 5:** Commit：`feat(mem-mcp): add HTTP client for mem service`

---

### Task 3: MCP 入口 + `memory_search` 工具

**Files:**
- Create: `integrations/mem-mcp/src/index.ts`
- Create: `integrations/mem-mcp/src/tools/search.ts`
- Modify: `integrations/mem-mcp/package.json`（`start` 指向编译产物）

- [ ] **Step 1: 写测试** — mock `fetch`，断言对 `POST .../memories/search` 的 body 含 `query`、`caller_agent`；或对 `registerTool` 的 handler 做集成级轻测（若过于笨重，可只测 `buildSearchBody` 纯函数）。

- [ ] **Step 2:** 实现 `memory_search`：参数与 `SearchMemoryRequest` 对齐（见 `src/domain/query.rs`）：`query`, `intent`, `scope_filters`（string[]）, `token_budget`, `caller_agent`, `expand_graph`, 可选覆盖 `tenant`（默认 `getConfig().tenant`）。请求体 `snake_case` 字段名与 Rust `serde` 一致。

- [ ] **Step 3:** `index.ts`：`McpServer` + `stdio` transport，注册 `memory_search`，handler 内调用 `memRequest`；返回 JSON 字符串或 MCP 结构化内容（按 SDK 推荐方式）。

- [ ] **Step 4:** 手动验收：终端 1 `MEM_DB_PATH=... cargo run`，终端 2 `MEM_BASE_URL=http://127.0.0.1:3000 npx .` 或通过 Cursor 连 MCP，调用 `memory_search` **Expected:** 返回与 `curl POST /memories/search` 一致形状。

- [ ] **Step 5:** `npm run test` **Expected:** PASS

- [ ] **Step 6:** Commit：`feat(mem-mcp): add memory_search MCP tool`

---

### Task 4: `memory_ingest` 与 `memory_get`

**Files:**
- Create: `integrations/mem-mcp/src/tools/ingest.ts`
- Create: `integrations/mem-mcp/src/tools/get.ts`
- Modify: `integrations/mem-mcp/src/index.ts`

- [ ] **Step 1:** 对照 `src/http/memory.rs` 与 `HttpIngestMemoryRequest`：`memory_ingest` 工具参数覆盖 ingest 所需字段（`tenant` 默认配置、`memory_type`, `content`, `scope`, `visibility`, `write_mode`, 可选 `evidence`, `code_refs`, `project`, `repo`, `module`, `tags`, `source_agent` 等）。**不要**发明新字段名；与 JSON 示例一致。

- [ ] **Step 2:** `memory_get`：`memory_id` 必填，`tenant` 可选 query；`GET /memories/{id}?tenant=`.

- [ ] **Step 3:** 测试：mock `fetch` 校验 path 与 method。

- [ ] **Step 4:** Commit：`feat(mem-mcp): add memory_ingest and memory_get tools`

---

### Task 5: 扩展工具 — feedback、pending、episode

**Files:**
- Create: `integrations/mem-mcp/src/tools/feedback.ts`
- Create: `integrations/mem-mcp/src/tools/pending.ts`
- Create: `integrations/mem-mcp/src/tools/episode.ts`
- Modify: `integrations/mem-mcp/src/index.ts`

- [ ] **Step 1:** `memory_feedback` → `POST /memories/feedback`，body：`tenant`, `memory_id`, `feedback_kind`（与 `FeedbackKind` 的 snake_case 一致）。

- [ ] **Step 2:** `memory_list_pending_review` → `GET /reviews/pending?tenant=`.

- [ ] **Step 3:** `episode_ingest` → `POST /episodes`；字段与 `IngestEpisodeRequest` 对齐（查阅 `src/domain/episode.rs`）。

- [ ] **Step 4:** 各工具 description 写清「何时用」，便于模型选型。

- [ ] **Step 5:** Commit：`feat(mem-mcp): add feedback, pending review, and episode tools`

---

### Task 6（可选）: Embeddings 维护工具

**Files:**
- Create: `integrations/mem-mcp/src/tools/embeddings.ts`
- Modify: `integrations/mem-mcp/src/config.ts`、`integrations/mem-mcp/src/index.ts`

- [ ] **Step 1:** 仅当 `process.env.MEM_MCP_EXPOSE_EMBEDDINGS === "1"` 时注册：`embeddings_list_jobs`（GET query）、`embeddings_rebuild`（POST body）、`embeddings_providers`（GET）。路径与 `src/http/embeddings.rs` 一致。

- [ ] **Step 2:** README 注明：默认不暴露，避免普通编码 agent 误触维护接口。

- [ ] **Step 3:** Commit：`feat(mem-mcp): optional embeddings admin tools`

---

### Task 7: Skill — `docs/superpowers/skills/mem-mcp-codex/SKILL.md`

**Files:**
- Create: `docs/superpowers/skills/mem-mcp-codex/SKILL.md`

- [ ] **Step 1:** 按 spec §5 写五段策略（开始前 search、进行中、写入策略、结束后 episode/ingest、`caller_agent` 约定）。

- [ ] **Step 2:** 表格列出环境变量：`MEM_BASE_URL`、`MEM_TENANT`、`MEM_MCP_EXPOSE_EMBEDDINGS`（可选）。

- [ ] **Step 3:** 链接到 `integrations/mem-mcp/README.md`（Cursor MCP 配置）与根 `README`。

- [ ] **Step 4:** Commit：`docs(skill): add mem MCP + Codex workflow skill`

---

### Task 8: 根 README + CI HTTP 示例

**Files:**
- Modify: `README.md`
- Create: `docs/superpowers/examples/ci-mem-http-snippet.md`

- [ ] **Step 1:** `README.md` 增加「Codex / MCP」：先起 `mem`，再配置 MCP 指向 `integrations/mem-mcp` 的 `start` 命令；指向 spec 与 Skill。

- [ ] **Step 2:** CI 示例文档：展示 `curl` 调用 `$MEM_BASE_URL/memories/search`（`caller_agent: ci`）与可选 `$MEM_BASE_URL/episodes`；说明与 MCP 共用同一 `tenant`。

- [ ] **Step 3:** Commit：`docs: link Codex MCP integration and CI HTTP examples`

---

### Task 9（可选）: 仓库 CI 跑 mem-mcp 测试

**Files:**
- Create 或 Modify: `.github/workflows/*.yml`（若仓库已有 CI 则追加 job）

- [ ] **Step 1:** Job：`checkout` → `cd integrations/mem-mcp` → `npm ci` → `npm test`。

- [ ] **Step 2:** Commit：`ci: run mem-mcp package tests`

---

## Plan review

- 将本 plan 与 spec 一并交给评审（人工或 plan-document-reviewer）；重点检查：**工具参数名与 Rust JSON 契约 1:1**、**默认 tenant 行为**、**错误信息长度上限**（避免 MCP 传巨型 HTML）。

---

## Execution handoff

Plan 已保存到 `docs/superpowers/plans/2026-03-21-codex-mem-mcp-integration.md`。

**执行方式二选一：**

1. **Subagent-Driven（推荐）** — 每任务派生子代理，任务间 review；使用 @superpowers/subagent-driven-development  
2. **Inline Execution** — 本会话按任务推进，使用 @superpowers/executing-plans  

你回复选用哪一种（或「从 Task 1 开始在本会话实现」）即可开干。
