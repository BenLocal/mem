# Shared Agent Memory Strategy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在不重构 Rust `mem` 服务的前提下，为 `mem-mcp` 增加面向多 coding agent 的高层共享记忆工具与默认策略，使多个 Codex / MCP 客户端能更稳定地共享、检索和写入记忆。

**Architecture:** 继续把 Rust `mem` 保持为通用 HTTP 后端，在 `integrations/mem-mcp` 中增加一层轻策略网关。新工具按任务意图拆分为 bootstrap search、contextual search、fact commit、experience proposal、preference proposal 和 feedback apply；底层仍调用现有 `/memories/search`、`/memories`、`/memories/feedback`、`/episodes`、`/reviews/*`。默认 project-first scope 与事实/经验/偏好分流优先在 MCP 层实现。

**Tech Stack:** Rust `mem` HTTP API、Node.js 20、TypeScript、`@modelcontextprotocol/sdk`、Vitest、现有 `integrations/mem-mcp` 包结构。

**Spec:** `docs/superpowers/specs/2026-03-23-shared-agent-memory-strategy-design.md`

---

## File map（创建 / 修改）

| 区域 | 路径 | 职责 |
|------|------|------|
| MCP 工具注册 | `integrations/mem-mcp/src/register-tools.ts` | 新增高层工具注册顺序与暴露控制 |
| MCP schema | `integrations/mem-mcp/src/schemas.ts` | 新增 `memory_kind`、scope policy、intent 相关 schema |
| MCP 配置 | `integrations/mem-mcp/src/config.ts` | 默认 `project` scope、可选 repo/personal 开关、默认 agent/source 策略 |
| MCP 客户端 | `integrations/mem-mcp/src/mem-client.ts` | 对现有 HTTP API 的 payload 组装帮助函数 |
| 搜索工具 | `integrations/mem-mcp/src/tools/search.ts` 或拆分新文件 | 实现 `memory_bootstrap`、`memory_search_contextual` |
| 写入工具 | `integrations/mem-mcp/src/tools/*.ts` | 实现 `memory_commit_fact`、`memory_propose_experience`、`memory_propose_preference` |
| 反馈工具 | `integrations/mem-mcp/src/tools/feedback.ts` | 暴露 `memory_apply_feedback` 高层别名或新工具 |
| 单测 | `integrations/mem-mcp/src/*.test.ts` / `src/tools/*.test.ts` | 覆盖 payload 默认值、scope 限制、write_mode 映射、错误处理 |
| Skill 文档 | `docs/superpowers/skills/mem-mcp-codex/SKILL.md` | 更新默认检索时机、写入分级、反馈习惯 |
| 包文档 | `integrations/mem-mcp/README.md` | 说明新高层工具、兼容策略、环境变量 |
| 根 README | `README.md` | 补充“多 agent 共享 + project-first 默认策略”说明 |

---

### Task 1: 盘点现有 MCP 工具与可复用映射

**Files:**
- Modify: `integrations/mem-mcp/src/register-tools.ts`
- Modify: `integrations/mem-mcp/src/mem-client.ts`
- Modify: `integrations/mem-mcp/src/schemas.ts`

- [ ] **Step 1: 列出现有原始工具与目标高层工具的一一映射**

记录以下映射表并写进计划执行笔记：
- `memory_bootstrap` -> `POST /memories/search`
- `memory_search_contextual` -> `POST /memories/search`
- `memory_commit_fact` -> `POST /memories`
- `memory_propose_experience` -> `POST /episodes` 或 `POST /memories`
- `memory_propose_preference` -> `POST /memories`
- `memory_apply_feedback` -> `POST /memories/feedback`

- [ ] **Step 2: 为每个高层工具定义最小输入字段**

至少包含：
- `tenant`
- `project`
- `repo?`
- `module?`
- `caller_agent` / `source_agent`
- `memory_kind`（若适用）

- [ ] **Step 3: 写一个最小失败测试，断言 bootstrap 搜索默认只用 project 级**

```ts
it("memory_bootstrap defaults to project-only scope", async () => {
  // 调用工具注册 handler，断言发往 mem 的 payload 不含 repo/personal 默认开启
});
```

- [ ] **Step 4: 运行测试，确认先失败**

Run: `npm test -- --runInBand memory-bootstrap`
Expected: FAIL，原因是工具或默认 payload 尚不存在

- [ ] **Step 5: 提交本任务**

```bash
git add integrations/mem-mcp/src/register-tools.ts integrations/mem-mcp/src/mem-client.ts integrations/mem-mcp/src/schemas.ts
git commit -m "test: define shared memory strategy tool contracts"
```

### Task 2: 实现 `memory_bootstrap`

**Files:**
- Modify: `integrations/mem-mcp/src/tools/search.ts`
- Modify: `integrations/mem-mcp/src/register-tools.ts`
- Test: `integrations/mem-mcp/src/tools/search.test.ts`

- [ ] **Step 1: 写 `memory_bootstrap` 的失败测试**

```ts
it("memory_bootstrap sends low-budget project-scoped search", async () => {
  // 断言 query 被透传，token_budget 为低默认值，scope 为 project-first
});
```

- [ ] **Step 2: 运行该测试，确认失败**

Run: `npm test -- search.test.ts -t "memory_bootstrap"`
Expected: FAIL，提示工具未注册或 payload 不匹配

- [ ] **Step 3: 实现最小工具逻辑**

要求：
- 复用现有 search 请求通道
- 自动补低 `token_budget`
- 默认只使用 project 范围
- 返回精简结果而不是泄露全部 HTTP 原始形状

- [ ] **Step 4: 运行测试确认通过**

Run: `npm test -- search.test.ts -t "memory_bootstrap"`
Expected: PASS

- [ ] **Step 5: 提交本任务**

```bash
git add integrations/mem-mcp/src/tools/search.ts integrations/mem-mcp/src/register-tools.ts integrations/mem-mcp/src/tools/search.test.ts
git commit -m "feat: add bootstrap memory search tool"
```

### Task 3: 实现 `memory_search_contextual`

**Files:**
- Modify: `integrations/mem-mcp/src/tools/search.ts`
- Modify: `integrations/mem-mcp/src/schemas.ts`
- Test: `integrations/mem-mcp/src/tools/search.test.ts`

- [ ] **Step 1: 写失败测试，覆盖 `intent` 与 scope 开关**

```ts
it("memory_search_contextual enables repo or personal only when explicitly requested", async () => {
  // 默认 project-only；显式 include_repo/include_personal 才放宽
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `npm test -- search.test.ts -t "memory_search_contextual"`
Expected: FAIL

- [ ] **Step 3: 实现最小逻辑**

要求：
- 接收 `intent`：`implementation` / `debugging` / `review`
- 显式控制 repo/personal 开关
- 保持对底层 `/memories/search` 的兼容

- [ ] **Step 4: 运行搜索相关测试**

Run: `npm test -- search.test.ts`
Expected: PASS

- [ ] **Step 5: 提交本任务**

```bash
git add integrations/mem-mcp/src/tools/search.ts integrations/mem-mcp/src/schemas.ts integrations/mem-mcp/src/tools/search.test.ts
git commit -m "feat: add contextual memory search tool"
```

### Task 4: 实现 `memory_commit_fact`

**Files:**
- Modify: `integrations/mem-mcp/src/tools/ingest.ts` 或新增 `src/tools/commit-fact.ts`
- Modify: `integrations/mem-mcp/src/register-tools.ts`
- Test: `integrations/mem-mcp/src/tools/commit-fact.test.ts`

- [ ] **Step 1: 写失败测试，断言 fact 默认走自动写入**

```ts
it("memory_commit_fact maps fact payload to auto ingest request", async () => {
  // 断言 write_mode=auto，source_agent 必填，结构化字段齐全
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `npm test -- commit-fact.test.ts`
Expected: FAIL

- [ ] **Step 3: 实现最小工具**

要求：
- 强制要求 `summary`、`content`、`evidence`
- 自动带 `write_mode=auto`
- 要求 project 上下文
- 对重复事实保持与现有后端幂等策略兼容

- [ ] **Step 4: 运行测试确认通过**

Run: `npm test -- commit-fact.test.ts`
Expected: PASS

- [ ] **Step 5: 提交本任务**

```bash
git add integrations/mem-mcp/src/tools/commit-fact.ts integrations/mem-mcp/src/register-tools.ts integrations/mem-mcp/src/tools/commit-fact.test.ts
git commit -m "feat: add fact commit tool"
```

### Task 5: 实现 `memory_propose_experience` 与 `memory_propose_preference`

**Files:**
- Modify: `integrations/mem-mcp/src/tools/episode.ts` 或新增 `src/tools/propose-experience.ts`
- Modify: `integrations/mem-mcp/src/tools/ingest.ts` 或新增 `src/tools/propose-preference.ts`
- Modify: `integrations/mem-mcp/src/register-tools.ts`
- Test: `integrations/mem-mcp/src/tools/proposals.test.ts`

- [ ] **Step 1: 先写两个失败测试**

```ts
it("memory_propose_experience does not map to strong auto-recall by default", async () => {
  // 断言它走 episode 或低权重候选路径
});

it("memory_propose_preference always enters review flow", async () => {
  // 断言 write_mode=propose
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `npm test -- proposals.test.ts`
Expected: FAIL

- [ ] **Step 3: 实现最小逻辑**

要求：
- `experience` 优先写 episode；若用 memories 路径也必须不是强事实自动活化
- `preference` 强制 `write_mode=propose`
- 两者都要求 `source_agent`

- [ ] **Step 4: 运行提议类测试**

Run: `npm test -- proposals.test.ts`
Expected: PASS

- [ ] **Step 5: 提交本任务**

```bash
git add integrations/mem-mcp/src/tools/propose-experience.ts integrations/mem-mcp/src/tools/propose-preference.ts integrations/mem-mcp/src/register-tools.ts integrations/mem-mcp/src/tools/proposals.test.ts
git commit -m "feat: add experience and preference proposal tools"
```

### Task 6: 实现 `memory_apply_feedback`

**Files:**
- Modify: `integrations/mem-mcp/src/tools/feedback.ts`
- Modify: `integrations/mem-mcp/src/register-tools.ts`
- Test: `integrations/mem-mcp/src/tools/feedback.test.ts`

- [ ] **Step 1: 写失败测试，断言高层工具会映射到现有 feedback API**

```ts
it("memory_apply_feedback forwards useful/outdated/incorrect feedback", async () => {
  // 断言 payload 与现有 API 契约兼容
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `npm test -- feedback.test.ts -t "memory_apply_feedback"`
Expected: FAIL

- [ ] **Step 3: 实现最小逻辑**

要求：
- 暴露直观命名
- 兼容现有 feedback kind
- 保留 HTTP 错误透传

- [ ] **Step 4: 运行反馈测试**

Run: `npm test -- feedback.test.ts`
Expected: PASS

- [ ] **Step 5: 提交本任务**

```bash
git add integrations/mem-mcp/src/tools/feedback.ts integrations/mem-mcp/src/register-tools.ts integrations/mem-mcp/src/tools/feedback.test.ts
git commit -m "feat: add memory feedback apply tool"
```

### Task 7: 更新 Skill 与文档

**Files:**
- Modify: `docs/superpowers/skills/mem-mcp-codex/SKILL.md`
- Modify: `integrations/mem-mcp/README.md`
- Modify: `README.md`

- [ ] **Step 1: 写文档测试清单**

确认文档至少说明：
- project-first 默认策略
- 何时用 bootstrap / contextual search
- fact / experience / preference 的写入分流
- 多 agent 共享时必须带 `source_agent`

- [ ] **Step 2: 更新 Skill**

在 Skill 中明确：
- 任务开始时轻量搜索
- 关键节点追加搜索
- 写入分级与 review 规则
- 反馈闭环习惯

- [ ] **Step 3: 更新包 README**

补充：
- 新高层工具表
- 与旧原始工具的区别
- 多 agent 共享的配置建议

- [ ] **Step 4: 更新根 README**

补充：
- 多 agent 共享的默认边界
- `MEM_BASE_URL` + `tenant` + `project` 的推荐组合

- [ ] **Step 5: 运行文档相关检查**

Run: `npm test`
Expected: 现有 mem-mcp 测试继续通过

- [ ] **Step 6: 提交本任务**

```bash
git add docs/superpowers/skills/mem-mcp-codex/SKILL.md integrations/mem-mcp/README.md README.md
git commit -m "docs: describe shared memory strategy defaults"
```

### Task 8: 全量验证与收尾

**Files:**
- Modify: `integrations/mem-mcp/src/**/*.test.ts`
- Modify: `.github/workflows/ci.yml`（仅在需要新增测试命令时）

- [ ] **Step 1: 运行 mem-mcp 全量测试**

Run: `cd integrations/mem-mcp && npm test`
Expected: PASS

- [ ] **Step 2: 运行 mem-mcp 构建**

Run: `cd integrations/mem-mcp && npm run build`
Expected: PASS

- [ ] **Step 3: 如有需要，补充 CI**

仅当新测试命令或构建步骤变化时修改 `.github/workflows/ci.yml`。

- [ ] **Step 4: 手工冒烟检查工具命名与 README 一致**

确认：
- 工具名
- 参数名
- 默认策略
- 与 spec 表述一致

- [ ] **Step 5: 最终提交**

```bash
git add integrations/mem-mcp docs/superpowers/skills/mem-mcp-codex/SKILL.md README.md .github/workflows/ci.yml
git commit -m "feat: add shared agent memory strategy tools"
```

---

## Plan review

由于当前会话未授权显式启用子 agent，本计划未走 plan-document-reviewer 子代理回路。执行前应人工复核以下点：

- `experience` 第一版到底落 `episode` 还是低权重 memory，需在实现开始前固定；
- `project` 字段若现有 API 未强制支持，MCP 层如何映射到现有 `scope` / `repo` / `module` 组合；
- 高层工具返回是否需要压缩结果形状，避免把原始 HTTP 响应直接暴露给模型。

## Execution handoff

Plan complete and saved to `docs/superpowers/plans/2026-03-23-shared-agent-memory-strategy.md`. Two execution options:

**1. Subagent-Driven (recommended)** - I dispatch a fresh subagent per task, review between tasks, fast iteration

**2. Inline Execution** - Execute tasks in this session using executing-plans, batch execution with checkpoints

Which approach?
