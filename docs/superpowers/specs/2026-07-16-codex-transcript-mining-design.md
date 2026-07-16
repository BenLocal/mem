# Codex 支持:`mem mine` 解析 Codex rollout + 插件清理

**日期**:2026-07-16
**状态**:设计已批准,待实现
**范围**:让 mem 在 Codex 与 Claude Code 下同时可用,修复集中在 **mem 二进制**;`.claude-plugin/` 一字不动。

---

## 1. 背景与根因

mem 作为插件同时供 Claude Code 与 Codex 使用。经查证(见本仓 `docs/architecture.md` 之外的实测):

- **Codex 的插件系统是 Claude Code 插件格式的超集兼容实现**。本地 marketplace 只认 `<repo>/.claude-plugin/marketplace.json`(Codex 二进制里无 `.codex-plugin/marketplace.json` 这条路径)。因此 Codex 安装并运行的是 mem 的 **`.claude-plugin/`** 变体;`.codex-plugin/` 目录结构上没有任何入口会被读到,是**死代码**。
- Codex 运行 Claude hook 没问题:它展开 `CLAUDE_PLUGIN_ROOT`、把 Claude 兼容的 JSON(`transcript_path`、`session_id`…)灌到 hook 的 stdin、把 Claude 事件名归一化成 snake_case。
- **但 `mem mine` 的 transcript 解析器写死了 Claude 的 JSONL 结构**(`src/cli/mine.rs::parse_transcript_full`:键 `value["type"] ∈ {user,assistant,system}` + `value["message"]["content"]`)。Codex 的 rollout 是完全不同的 schema,导致 `mem mine` 对 Codex 会话**一行都提取不出来** —— 这是 Stop/PreCompact 挖矿在 Codex 上空转的真因。

各场景在 Codex 的现状:

| 场景 | 现状 | 原因 |
|---|---|---|
| SessionStart 唤醒 / UserPromptSubmit 召回 | ✅ 正常 | 只查记忆,不碰 transcript |
| PreCompact 挖矿 | ❌ 空 | `mem mine` 解析不了 Codex rollout(shell 无节流,会调用) |
| Stop 挖矿 | ❌ 不触发 | 节流计数在 `.claude-plugin` 的 shell 里用 `grep -c '"type":"user"'`,Codex rollout 上恒为 0 → 永不达阈值 |
| 记忆 agent 标签 | ❌ 标成 `claude-code` | Claude hook 硬编码 `--agent claude-code` |
| MCP 工具(`capability_capsule_*`) | ❌ 缺失 | 插件 mcpServers 未落到 `~/.codex/config.toml` `[mcp_servers.*]` |

## 2. 目标与非目标

**目标**
- `mem mine` 能解析 Codex rollout 格式,提取记忆 + 归档 block 与 Claude 路径等价。
- Codex 会话挖出的记忆自动标 `source_agent=codex`。
- Codex 里 `capability_capsule_*` MCP 工具可用。
- 删除死的 `.codex-plugin/`。

**非目标 / 边界**
- **不改 `.claude-plugin/`**(回归保护)。直接后果:Stop 钩子的 shell 节流仍按 Claude 结构计数,**Stop-on-Codex 仍不触发**;由 **PreCompact 兜底**(压缩前无条件挖一次)。此限制是"不动 `.claude-plugin`"的明确代价,已接受。
- 不做 `.codex-plugin/` 的远程 curated 打包(已决定删)。
- 不改 mem 的存储/检索/其它管道。

## 3. Codex rollout schema(实测)

每行:`{"type": <top>, "payload": {...}, "timestamp": "..."}`。

- 顶层 `type` ∈ `{session_meta, event_msg, response_item, turn_context, world_state, compacted}`。**只有 `response_item` 承载可挖的会话内容。**
- `response_item.payload.type`:
  - `message`:`{type:"message", role:"user"|"assistant"|"developer", content:[{type:"input_text"|"output_text", text}]}`
  - `reasoning`:`{type:"reasoning", summary:[{type:"summary_text", text}], content:null, encrypted_content:<opaque>}`(人读文本在 `summary[].text`;可能为空)
  - `function_call`:`{type:"function_call", name, arguments:<json-string>, call_id, id}`
  - `function_call_output`:`{type:"function_call_output", call_id, output:<string>}`

## 4. 设计

### 4.1 格式探测(`src/cli/mine.rs`)

新增 `enum TranscriptFormat { ClaudeCode, CodexRollout }` + `fn detect_format(path) -> TranscriptFormat`:

- 读首个可解析行:若 `value["type"] == "session_meta"`,或该行同时含 `payload` 对象且 `type` 不在 `{user,assistant,system}` → `CodexRollout`。
- 否则 → `ClaudeCode`(现有行为,默认)。
- 探测失败/空文件 → `ClaudeCode`(fail-safe,保持旧路径)。

### 4.2 Codex 解析分支

`parse_transcript_full` 顶部按 `detect_format` 分派;Claude 路径完全不动。新增 `parse_codex_rollout(path, ...)`,逐行只处理 `type=="response_item"`,按 `payload.type` 映射到**与 Claude 路径相同的** `ArchivedBlock` / block 结构:

| Codex `payload.type` | role | block_type | content 来源 |
|---|---|---|---|
| `message` role=`user` | user | text | 拼接 `content[].text`(input_text) |
| `message` role=`assistant` | assistant | text | 拼接 `content[].text`(output_text) |
| `message` role=`developer` | system | text | 拼接 `content[].text` |
| `reasoning` | assistant | thinking | 拼接 `summary[].text`(空则跳过) |
| `function_call` | assistant | tool_use | `name` + `arguments` |
| `function_call_output` | user | tool_result | `output` |

- `<mem-save>` 提取与启发式抽取沿用现有逻辑(它们作用在 assistant text block 上,映射后自然生效)。
- 归档 block 的 `embed_eligible` 沿用现有规则(`text` / `thinking`)。
- block_id/session 等字段沿用 caller 语义,避免跨 replay 重铸(参照仓内既有约定)。

### 4.3 来源 agent 跟随格式

`source_agent` 逻辑:探测到 `CodexRollout` → `source_agent = "codex"`(**覆盖** hook 传入的 `--agent claude-code`);`ClaudeCode` → 尊重 `--agent`(默认 `claude-code`)。落地时 `warn!`/`info!` 一行说明覆盖,便于排查。理由:hook 脚本不可改(约束),来源必须由 mem 自判。

### 4.4 MCP 注册(`~/.codex/config.toml`,用户机器)

追加:

```toml
[mcp_servers.mem]
command = "mem"
args = ["mcp"]

[mcp_servers.mem.env]
MEM_BASE_URL = "http://127.0.0.1:3000"
MEM_TENANT = "local"
```

（幂等:已存在则不重复添加。）

### 4.5 删除 `.codex-plugin/`

删整个目录 + 清理 repo 内引用(README、docs、打包脚本里若提及)。grep `\.codex-plugin` 全仓确认无残留引用。

## 5. 测试

- **单测(mine.rs 内联)**:`detect_format` 对 Codex/Claude 两种首行的判定;`parse_codex_rollout` 对一个内嵌的最小 rollout fixture(含 message×user/assistant/developer + function_call + function_call_output + reasoning)产出预期 blocks + role/block_type 映射 + `source_agent=codex`。
- **集成测试(`tests/cli_mine.rs`)**:放一个 Codex rollout fixture 文件,`mem mine <fixture> --format hook-stop` 断言提取/归档非空且 agent=codex;Claude fixture 回归断言不变。
- **CI 门**:`cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test`。
- **手工验证**:用真实 `~/.codex/sessions/.../rollout-*.jsonl` 跑一遍,确认非空提取、agent=codex;`codex plugin list` 仍正常;Codex 里 MCP 工具可见。

## 6. 风险

- **reasoning.summary 常为空/加密**:映射时空则跳过,不产空 thinking block。低风险。
- **rollout schema 版本漂移**:字段名若随 Codex 版本变(如 `input_text`→别的),解析产空。缓解:探测 + 解析都对未知 payload.type 静默跳过(不 panic),并保留 Claude 路径不受影响。
- **agent 覆盖的意外**:若将来有人手工 `mem mine <codex-rollout> --agent X` 想标别的,会被强制成 codex。属边界,可接受;必要时后续加 `--agent-force`。

## 7. 变更文件清单

| 文件 | 改动 |
|---|---|
| `src/cli/mine.rs` | 格式探测 + Codex 解析分支 + agent 跟随格式 + 内联单测 |
| `tests/cli_mine.rs` | Codex rollout fixture 集成测试 + Claude 回归 |
| `~/.codex/config.toml` | 加 `[mcp_servers.mem]`(用户机器,非仓库) |
| `.codex-plugin/`(删除) | 整目录 + 清理引用 |
