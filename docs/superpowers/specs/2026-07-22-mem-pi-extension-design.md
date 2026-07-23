# mem × pi 集成设计

**日期**: 2026-07-22
**状态**: 已批准,待实现计划
**关联**: [[Codex 插件加载 & mine 双格式]]（2026-07-16 codex-transcript-mining-design.md，本设计是它的 pi 版对应）

---

## 1. 背景与目标

mem 目前通过 **Claude Code plugin**（`.claude-plugin/`，hooks + MCP）和 **Codex plugin**（同一 `.claude-plugin`，运行时自适应）被两个 agent 宿主消费。第三个宿主 **pi**（`@earendil-works/pi-coding-agent` v0.74.2，本机 `/root/.nvm/.../bin/pi`）用完全不同的扩展模型：

- **无 hook 系统** —— 用事件订阅 `pi.on(event, handler)`。
- **无 MCP 支持**（v0.74.2 零 MCP 依赖 / 客户端 / 配置，已核实）—— 工具只能靠扩展内 `registerTool()` 得到。
- **扩展 = TypeScript 文件**，经 `pi install <source>` 写入 settings，运行时 `node --experimental-strip-types` 加载。

目标：一个 pi 扩展 + 少量 mem Rust 改动，让 mem 在 pi 中达到与 Claude/Codex 同等的能力：进程生命周期托管、工具可用、唤醒上下文注入、自动挖矿、反馈闭环。

### 关键架构约束（探索得出，决定设计）

1. **pi 事件传入的是内存 `AgentMessage[]`，不是 transcript 路径**（`agent_end.messages` / `session_shutdown` / `message_end.message`）—— 与 Claude Code hook 从 stdin 拿 `transcript_path` 根本不同。但 pi **确实**持久化会话到 `~/.pi/agent/sessions/<cwd-slug>/<ts>_<uuid>.jsonl`，且扩展可通过 `ctx.sessionManager.getSessionFile()` 拿到当前会话文件路径。
2. **pi 会话文件是第三种格式**：首行 `{"type":"session","version":N,...}`，随后 `model_change` / `thinking_level_change` / `message` 行；消息行形如 `{"type":"message","id":"6e055cf0","parentId":..,"message":{"role":"user|assistant","content":[{"type":"text","text":..}|工具块],"timestamp":..}}`。既非 Claude（`{type:user/summary}`）也非 Codex（`{type:response_item,payload}`）。
3. **`pi.exec()` 阻塞到命令结束**（返回 `ExecResult{stdout,stderr,code,killed}`，无 detached 选项）—— 不能用它常驻 `mem serve`；守护进程用 Node `child_process.spawn(..,{detached:true}).unref()`。短命令（wake-up / mine / feedback / health）才用 `exec()` 或 `fetch()`。

---

## 2. 架构总览

扩展是唯一新入口，通道如下：

```
pi 会话 ──(事件)──> mem-extension.ts ──┬─ node spawn ──> mem serve 守护进程 (生命周期)
                                       ├─ node spawn ──> mem mcp 子进程 (工具代理，stdio JSON-RPC)
                                       │                     └─ HTTP :3000 ─> mem serve
                                       ├─ HTTP :3000 ──> mem serve (health / wake-up / 自动召回 search)
                                       └─ pi.exec CLI ─> mem wake-up / mine / feedback-from-transcript
```

两件交付物：

- **交付物 A（mem 侧 Rust）**：给 `mem mine` + `mem feedback-from-transcript` 加 **pi transcript 格式解析**（第三分支，和 Codex 并列）。**这是唯一的 mem 侧改动**。
- **交付物 B（pi 扩展）**：`packaging/pi/mem-extension.ts` + `package.json` —— serve/mcp 生命周期 + 事件 + **MCP 子进程代理暴露 ~40 工具** + wake-up/自动召回注入。

### 工具暴露机制（决策 2026-07-22：MCP 子进程代理，非 HTTP 直连 codegen）

~40 个 mem 工具里多个是**动态路径**（`capability_capsule_get`/`entity_get`/`graph_neighbors`/`embeddings_list_jobs` 等在 `src/mcp/server.rs` 里现拼 `capability_capsules/{id}` 这类 path），静态路由表拼不出。故扩展**不**做 HTTP 直连 + manifest codegen，改为：扩展拉起 `mem mcp` 子进程，用极简 MCP stdio 客户端发 `tools/list` 运行时发现全部工具 + `inputSchema`（**直接来自 rmcp router，零漂移、零手维护**），逐个 `registerTool`；每个 `execute` 把 `tools/call` 转发给子进程，由已写好的 `server.rs` 负责全部路径拼接/参数默认/转发 HTTP 到 `mem serve`。仍严格满足「每个 MCP 工具 → 一个 pi registerTool()」，只是 execute 转发给 `mem mcp` 而非手拼 HTTP。

---

## 3. 交付物 A — mem Rust 改动

### 3.1 pi transcript 格式解析（`cli/mine.rs` + `cli/feedback.rs`）

- **格式识别**：读首行，`{"type":"session","version":N}` → pi 格式。三分支识别（Claude / Codex / pi），与现有 Codex 分支并列，落在 `cli/mine.rs` 的 transcript loader 与 `cli/feedback.rs::scan_transcript`。
- **解析**：迭代 `type=="message"` 行，读 `.message.role` + `.message.content[]`（text / tool_use / tool_result 块）→ 映射到 Claude/Codex 共用的内部 block 表示，后续挖矿/反馈逻辑不变。
- **block_id 稳定性**：用 pi 每行自带的 `.id`（如 `6e055cf0`）作为双-sink 归档的 block_id 源。pi 的 `.id` **跨 replay 稳定**，天然规避 [[mem transcript block_id 每次重铸]] 的重铸坑（比 Codex 需现造 id 更省心）。**必须**用存量 id，不得现铸。
- **agent 标记**：强制 `agent=pi`（对齐 Codex 强制 `agent=codex`）。

> **注**：原 §3.2「工具 manifest 导出（`mem mcp --dump-tools`）」已删除。2026-07-22 决策改用 **MCP 子进程代理**（见 §2「工具暴露机制」）：工具 schema 由扩展运行时对 `mem mcp` 发 `tools/list` 直接获得，无需 mem 侧新增导出命令。pi transcript 解析是**唯一**的 mem 侧改动。

---

## 4. 交付物 B — pi 扩展 `mem-extension.ts`

### 4.1 进程生命周期（决策：起-if-down；只杀本会话亲起的）

两个被托管进程：**`mem serve`**（共享守护，跨会话可复用）与 **`mem mcp`**（本会话工具代理子进程，每会话独占）。

- `on("session_start")`：
  - **serve**：`fetch(GET http://127.0.0.1:3000/health)`。不通 → `spawn("mem",["serve"],{detached:true,stdio:"ignore",env}).unref()`；记 `serveStartedByUs=true` + `pid`；轮询 health 至 up（超时 ~10s，退避）。端口已通 → 复用，`serveStartedByUs=false`。
  - **mcp 代理**：serve up 后 `spawn("mem",["mcp"],{stdio:["pipe","pipe","ignore"],env})`（**非** detached —— 它是本会话的子进程，随会话生灭）；MCP `initialize` 握手 → `tools/list` → 逐个 registerTool（见 §4.3）。
- `on("session_shutdown")`：
  - **mcp 代理**：无条件 `process.kill(mcpPid, SIGTERM)`（本会话独占）。
  - **serve**：仅当 `serveStartedByUs` → `process.kill(servePid, SIGTERM)`。**不误杀** supervisord 托管或其他会话在用的实例。
- 归属态存扩展模块级变量（每 pi 会话独立 runtime）。

### 4.2 wake-up 注入

- `on("session_start")`（serve up 后）：`pi.exec("mem",["wake-up"],{cwd})` → stdout → `sendUserMessage(stdout)` 注入。落进会话文件，利于反馈 round-trip。exec 失败 → warn 吞,不阻断会话。

### 4.3 工具（~40 registerTool，全量，MCP 子进程代理）

- **极简 MCP stdio 客户端**（扩展内 ~150 行）：对 §4.1 拉起的 `mem mcp` 子进程读写 JSON-RPC over stdio（换行分隔 / Content-Length 帧，依 rmcp `transport-io` 实际帧格式，实现时确认）。方法：`initialize` 握手 → `tools/list` → `tools/call`。请求按 `id` 关联 Promise，`stdout` 累积按帧拆分派发。
- **运行时发现 + 注册**：`tools/list` 返回 `[{name, description, inputSchema(JSON Schema)}]`（直接来自 rmcp router，零漂移）。逐个 `registerTool({name, description, parameters, execute})`：
  - `parameters`：pi 的 `TSchema` 就是 JSON Schema 的 TypeBox 超集；`inputSchema` 直接作为 `parameters` 传入（必要时用 `Type.Unsafe`/最小包装，实现时确认 pi 对裸 JSON Schema 的接受度）。
  - `execute(id, params, signal, onUpdate, ctx)`：`mcpClient.callTool(name, params)` → 把 `CallToolResult` 的 content 映射为 pi `AgentToolResult`。
- **tenant**：`mem mcp` 已从 `MEM_TENANT`（默认 `local`）自行填充，扩展不重复处理（对齐现有 MCP wrapper 行为）。
- `tools/list` 失败 / 子进程起不来 → warn，跳过工具注册，会话其余功能（生命周期/事件）正常。

### 4.4 事件 → hook 替代（决策：反馈→agent_end；提取→compact+shutdown）

- `on("agent_end")` → `pi.exec("mem",["feedback-from-transcript", getSessionFile()])`
- `on("session_before_compact")` + `on("session_shutdown")` → `pi.exec("mem",["mine", getSessionFile()])`
- 会话文件路径 = `ctx.sessionManager.getSessionFile()`。为空（`--no-session` ephemeral）→ 跳过 mine/feedback。

### 4.5 反馈环闭合 —— 自动召回（决策：采纳推荐，加 before_agent_start）

- Claude 的 `feedback-from-transcript` 靠扫**召回 banner**（UserPromptSubmit 自动召回注入的）给记账。pi 需同源提供 banner，否则 feedback 无东西可记。
- `on("before_agent_start")`（有 `prompt` 字段）→ 用 prompt 查 `mem search`（HTTP）→ 注入**和 Claude 同格式的 recall banner**（`MEM_RECALL_STYLE` 默认 `index` 形态）→ 落进会话文件。
- **格式耦合**：banner 渲染格式被 `cli/feedback.rs::scan_transcript` 反解析（见 [[Feedback loop ↔ CC transcript format dep]]）。pi 注入的 banner 必须与 scan_transcript 的解析器同步；§3.1 的 pi 解析分支喂出 block 文本后，scan_transcript 的既有 banner 解析即可复用。round-trip 测试同步更新。

---

## 5. 打包 / 安装

- 放 mem 仓 `packaging/pi/`（对齐现有 `packaging/npm`）。
- `package.json`：`pi.extensions:["./mem-extension.ts"]`，keyword `pi-package`。扩展是单文件 + 内联的极简 MCP 客户端，无 codegen 产物。
- 安装：`pi install <path-or-source>` → 写进 pi settings。运行时 pi 用 `node --experimental-strip-types` 加载。

## 6. 错误处理（全 fail-safe，对齐 mem 各 lane）

- serve 起不来 / health 超时 → warn，工具照常尝试（mem 可能外部托管），**不 crash 会话**。
- `exec mine/feedback/wake-up` 非零退出 → warn + 吞。
- `getSessionFile()` 空 → 跳过依赖 transcript 的动作。
- 自动召回 `mem search` 失败 → 跳过 banner 注入，会话正常继续。

## 7. 测试

- **Rust**：pi-format 解析单测（RED→GREEN，喂真实 pi 会话 JSONL fixture）；`feedback-from-transcript` banner round-trip 补 pi 分支。
- **扩展**：MCP stdio 客户端（帧拆分 / id 关联 / tools/list 解析）单测（可对一个 echo/stub 子进程或录制的帧字节）；生命周期（health-gated serve spawn、mcp 子进程随会话生灭、ownership-kill 只杀亲起的 serve）；事件路由。用 pi 的 print/rpc 模式或最小 harness 驱动。

## 8. 非目标 / YAGNI

- 不改 `.claude-plugin`（Claude/Codex 路径不动）。
- pi 无内建 MCP 客户端：扩展自带一个**极简、单向服务于本用途**的 MCP stdio 客户端，不追求通用 MCP 客户端完备性（只需 initialize/tools/list/tools/call）。
- 不做 HTTP 直连 wrapper / manifest codegen（动态路径工具拼不出，见 §2）。
- 不做 per-message 提取（message_end 每条触发，过频）—— 提取只在 compact + shutdown。
- 交付物 A 的 pi 解析分支使 pi 挖矿可独立于扩展运行（如批量 `mem mine` over pi 会话），但本设计不含批量 cron。

---

## 9. 关键映射速查

| Claude Code hook | pi 事件 | 动作 |
|---|---|---|
| SessionStart | `session_start` | serve 起-if-down + `mem wake-up` 注入 |
| UserPromptSubmit（自动召回） | `before_agent_start` | `mem search` → 注入 recall banner |
| Stop | `agent_end` | `mem feedback-from-transcript <session file>` |
| PreCompact + Stop | `session_before_compact` + `session_shutdown` | `mem mine <session file>` |
| （无） | `session_start` / `session_shutdown` | 拉起 / 杀本会话的 `mem mcp` 子进程 |
| （无） | `session_shutdown` | 杀本会话亲起的 `mem serve` |
| MCP server | `registerTool()` ×~40（转发给 `mem mcp` 子进程，运行时 `tools/list` 发现） | 工具可用 |
