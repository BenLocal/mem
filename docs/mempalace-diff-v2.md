# MemPalace × mem 对照（v2 — 2026-05-12 MCP 表面层重审）

> 这一篇是 `mempalace-diff.md`（v1）的补充。v1 的结论是 §8 路线图 #1–#15 全部 ✅，剩下都是"有意保留的形态/哲学差异"。**本篇不否认那个结论**——v1 列出的路线项确实都吃下了。
>
> 但 v1 的比较粒度是**架构 / 数据模型 / 检索流水线**层面；它没有逐个对照两边的 **MCP 工具表面**。本篇做了那个事：把 MemPalace 当前（`mempalace/mcp_server.py` HEAD）暴露的 29 个 MCP 工具一条条映射到 mem 的对应物，结果发现仍有几处**实质性缺口**——既不属于 §15.4（已论证拒绝），也不在 v1 §8 路线图里。
>
> 维护原则同 v1：本篇与代码不一致时**以代码为权威**；落地一项后回到对应表格更新状态（✅ done / 🚧 in progress）。

---

## 0. 方法论

| 步骤 | 来源 |
|---|---|
| 列 MemPalace 当前 MCP 工具 | `grep -E '^def tool_' /root/workspace/master/mempalace/mempalace/mcp_server.py` |
| 列 mem 当前 MCP 工具 | `src/mcp/server.rs` 内 `#[tool]` 标注 + 本仓库 `.claude-plugin/.mcp.json` 暴露的工具名 |
| 映射 | 按"功能等价"匹配，不按命名（mem 的 `capability_capsule_*` ≡ MemPalace 的 `drawer` 概念） |
| 分类 | (A) 已等价吸收 (B) 部分覆盖 (C) 真正缺口 (D) §15.4 已拒绝 |

非 MCP 表面（CLI / hook / SKILL.md）这一层 v1 已经详尽对照，本篇不重复。

---

## 1. 一句话结论

> **路线图层面无缺口；MCP 表面层有 5 个值得补的工具**——集中在 **KG 多跳 / 时序导出**与**浏览路径**两块，所有数据 schema 都已就位，缺的是把现有 repo / service 能力**暴露成 MCP 工具**。
>
> 不补：dedup probe、AAAK、Wing/Drawer 命名、L0–L3 唤醒栈、服务端 LLM 精排（这些在 §15.4 / 本篇 §2.8 都已论证）。

---

## 2. MCP 工具表面对照

> 行按 MemPalace 工具排序；mem 列写明对应物 + 状态。"映射强度"列：✅ 等价 / ⚠️ 部分覆盖 / ❌ 缺 / 🚫 故意不做。

### 2.1 内容读写

| MemPalace | mem 对应 | 映射强度 | 备注 |
|---|---|---|---|
| `tool_add_drawer` | `capability_capsule_ingest` / `_batch_ingest` | ✅ | mem 多了批量 + idempotency_key |
| `tool_get_drawer(id)` | `capability_capsule_get(id)` | ✅ | |
| `tool_delete_drawer(id)` | — | ⚠️ | mem 用 `feedback_kind=incorrect` 转 `status=Archived` 实现软删；硬删暂未暴露 |
| `tool_update_drawer(id, content?, wing?, room?)` | — | ⚠️ | mem 走 supersedes（新行 + `supersedes_capability_capsule_id` 链）；功能等价但不是 in-place |
| `tool_list_drawers(wing, room, limit, offset)` | — | ❌ | **缺**：浏览路径。mem 只有 `_search`（必填 query）。repo 层有 `list_capability_capsules_for_tenant` 但**未暴露**，且不支持 `project / repo / module / type` 过滤、不支持游标 |
| `tool_check_duplicate(content, threshold)` | — | ❌ | **缺**：写前 dedup probe。mem 有 `idempotency_key` + `content_hash` 但只对**写入方**生效，没有 read-only "这条已存吗" 探针 |

### 2.2 搜索 / 召回

| MemPalace | mem 对应 | 映射强度 | 备注 |
|---|---|---|---|
| `tool_search(query, wing?, room?, k)` | `capability_capsule_search` | ✅ | mem 多了 RRF + lifecycle re-rank + 四段式输出 |
| `tool_search` 上下文版 | `capability_capsule_search_contextual` | ✅ | mem 独有 |
| — | `capability_capsule_bootstrap` | — | mem 独有（首屏拉取） |

### 2.3 图层（**重点缺口区**）

| MemPalace | mem 对应 | 映射强度 | 备注 |
|---|---|---|---|
| `tool_traverse_graph(start_room, max_hops)` | `capability_capsule_graph_neighbors(node_id)` | ⚠️ | **缺多跳**。mem 当前固定 1-hop（`src/storage/lance_store/graph.rs::neighbors`）。数据 schema（`graph_edges`）支持深走，缺的是 `max_hops` 参数 + 递归遍历 |
| `tool_find_tunnels(wing_a, wing_b)` | — | ❌ | **缺**：跨 scope 桥发现 |
| `tool_create_tunnel / _list / _delete / _follow_tunnels` | — | ❌ | **缺**：caller-curated 链接。mem 现在所有边都由 `extract_graph_edges` 自动产出，没有"用户 / agent 手动建立 A ↔ B 关联"的通道 |
| `tool_kg_query(entity, as_of, direction)` | — | ⚠️ | **部分缺**：mem 有 `graph_neighbors(node)` 但**没有 `as_of` 时间点参数**——尽管 schema 上 `graph_edges.valid_from` / `valid_to` 完整存在。CLAUDE.md 曾提"`neighbors_at(node, ts)` supports point-in-time lookups"，**但 grep `neighbors_at` 全 repo 0 命中——文档漂移，实现没跟上** |
| `tool_kg_timeline(entity)` | — | ❌ | **缺**：实体时序视图（"项目 X 各阶段属于谁、用哪些标签"）。数据全在 `graph_edges` 里 |
| `tool_kg_invalidate(subj, pred, obj, ended)` | — | ❌ | **缺**：直接关边。mem 现在只在 supersedes 路径**间接**关 (`close_edges_for_memory`)，没有"我现在知道这条事实从 T 时刻起不成立"的 API |
| `tool_kg_add(subj, pred, obj, valid_from)` | — | ⚠️ | mem 通过 ingest 路径触发 `extract_graph_edges` 自动入图，但 caller **不能直接写边**——拉低了对 KG 的可控性 |
| `tool_graph_stats` | — | ❌ | **缺**：KG 聚合（node / edge 数、密度、按 kind 分布）。运维 / 可观测性有价值 |

### 2.4 浏览 / 导航

| MemPalace | mem 对应 | 映射强度 | 备注 |
|---|---|---|---|
| `tool_status` | `mem_health` | ⚠️ | mem 只回 `{reachable, health_body}`，没有 capsule count / 各 status 分布 |
| `tool_list_wings` | — | ❌ | **缺**：列 distinct `project` / `repo`。等价于一个 `SELECT DISTINCT project FROM capability_capsules` |
| `tool_list_rooms(wing)` | `transcripts_list_sessions` | ⚠️ | mem 这一侧只覆盖 transcript sessions（本会话刚加），capsule 的 sessions 等价物（按时间桶分组）没单独暴露 |
| `tool_get_taxonomy` | — | ❌ | **缺**：一次性拿到 `{wings: [...], rooms: {...}}` 全貌——MCP 客户端做导航 UI 用得到 |
| `tool_memories_filed_away` | — | — | mem 写是同步 200 返回，没有"延迟归档确认"的语义需求 |

### 2.5 实体（mem 这边更强）

| MemPalace | mem 对应 | 映射强度 | 备注 |
|---|---|---|---|
| 隐含在 `entity_detector.py` + `entity_registry.py`，**无 MCP 工具** | `entity_create` / `_get` / `_list` / `_add_alias` | ✅ + 反超 | mem 主动暴露成 MCP，MemPalace 反而没有 |

### 2.6 Agent 自留区（**真正缺口**）

| MemPalace | mem 对应 | 映射强度 | 备注 |
|---|---|---|---|
| `tool_diary_write(agent_name, entry, topic, wing)` | — | ❌ | **缺**：每个 caller_agent 的私有笔记，不进共享 capsule 池 |
| `tool_diary_read(agent_name, last_n, wing)` | — | ❌ | 同上读侧 |

**为什么这是真缺口**：mem 当前所有 capsule 都进同一 tenant 池，`caller_agent` 只是 tag。"agent 自己试错过程笔记 / 自言自语"如果走 ingest 就**污染共享检索**，如果不存就丢；目前唯一替代是写 `<mem-save>` tag 然后看 mine 抽不抽——不稳定也不该走那条路。

### 2.7 mem 独有的工具（v1 §15.3 已列，再过一遍）

- `capability_capsule_apply_feedback` / `_feedback` —— 5 种 feedback_kind → confidence / decay 调权
- `capability_capsule_propose_experience` / `_propose_preference` / `_commit_fact` —— 提案 → review 队列
- `capability_capsule_list_pending_review` / `_review_accept` / `_review_edit_accept` / `_review_reject` —— 评审流
- `embeddings_list_jobs` / `_providers` / `_rebuild` —— 嵌入运维（`MEM_MCP_EXPOSE_EMBEDDINGS=1` 才暴露）
- `episode_ingest` —— 多步任务记录
- `transcript_session_get` / `transcripts_list_sessions` / `transcripts_search` / `transcripts_range`（本会话刚加）—— 对话归档

这一栏是 v1 §15.3 + 本会话新增的合并视图，MemPalace 完全没有等价。

### 2.8 §15.4 已论证不引入（再列一遍提醒）

| MemPalace | 不做的理由 |
|---|---|
| `tool_get_aaak_spec` | mem 是 HTTP 服务，caller 已有自己的 LLM；服务端再烧一次 = 双重计算双重账单 |
| Wing / Drawer 命名 | `project` / `repo` / `module` + `capability_capsules` 行已是等价物，改名只增加心智负担 |
| L0–L3 唤醒栈在服务端 | mem 通过 `mem wake-up` 注入 SessionStart，让 caller 决定怎么用 |
| `tool_hook_settings` / `_reconnect` | 不适用：mem hook 走 env vars；MCP 是无状态转发不需要 reconnect |

---

## 3. 推荐路线（#16–#20，沿用 v1 §8 编号风格）

> 编号继续 v1 §8（最后是 #15），新条目从 #16 起，commit 引用格式：`(closes mempalace-diff-v2 #N)`。

| # | 题目 | 改动面 | 工作量估 | 优先级 |
|---|---|---|---|---|
| **#16** ✅ | **KG 多跳 + 时序导出**：`graph_neighbors` 加 `max_hops` + `as_of` 参数；新加 `kg_timeline(entity)` 和 `kg_invalidate(subj, pred, obj, ended)`；同时**修复 `neighbors_at` 文档漂移**（要么实现，要么从 CLAUDE.md 删） | 1 个 repo 方法签名扩展 + 2 个新 repo 方法 + service + http + mcp | ~6h | **P0** — 数据全在，纯暴露 |
| **#17** ✅ | **`graph_stats` MCP**：节点 / 边总数、按 kind 分布、平均度数、`valid_to IS NULL` 活跃边比例 | 1 个 repo SQL + service + http + mcp | ~2h | P1 — 运维可观测性 |
| **#16.1** ✅ | **`kg_add_edge` MCP**：caller-supplied 直接写边（不走 ingest 路径自动抽取），保留 caller 的 `valid_from`，幂等于 active `(from, to, relation)` | 1 个新 LanceStore 方法 + service + http + mcp | ~1h | 由 #16 同 PR 顺手做了 |
| **#18** ✅ | **浏览路径**：`capability_capsule_list_in_scope(project?, repo?, module?, capability_capsule_type?, status?, cursor, limit)` MCP + HTTP；不需要 embedding 命中即可看 | repo SQL 加 filter + cursor + service + http + mcp | ~4h | P1 — 解决"列 project X 下所有 capsule"的真实需求 |
| **#19** ✅ | **Agent diary**：MCP `agent_diary_write(caller_agent, content, topic?)` / `_read(caller_agent, last_n)`；底层走 capsule 表 + `capability_capsule_type=diary` + 默认从 search 路径**排除**；read 端按 `source_agent` 过滤 | 1 个新 `CapabilityCapsuleType::Diary` 变体 + 3 处 hybrid_candidates SQL 加 `!= 'diary'` + 2 MCP + retrieve `memory_type_score` 加 Diary→0 兜底 | ~3h | P1 — 解决"agent 自言自语不污染主池" |
| **#20** ✅ (phase A) | **User tunnels**（caller-curated 跨 scope 链接）：以 `relation` 字符串前缀 `user_tunnel:<label>` 作为约定（不动 schema）；新加 `kg_list_user_tunnels` MCP 用 `relation LIKE 'user_tunnel:%'` 过滤；create / delete 复用 #16 的 `kg_add_edge` / `kg_invalidate_edge` | 1 个新 repo 方法 + service + http + mcp | ~2h | Phase A 落地，避免 schema 迁移；Phase B（`origin` 列 + retrieve boost）继续延后 |

### 决策点

- **现在做**：#16 + #17 一个 PR 落地（KG 这组工具数据全有，最高 ROI），#18 / #19 各一 PR。#20 推迟到 #16–#19 落地后观察。
- **不做**：dedup probe（无编号）—— 当前 `idempotency_key` + `content_hash` 已能解决 90% 写入侧 dedup 需求，read-only probe 用 `_search` query=content[:80] 就能近似实现，单独工具回报有限。

---

## 4. 实施清单（#16 详化，给执行者参考）

> 把 KG 那一组拆得细一点，落地时按这个 checklist 走。

### 4.1 multi-hop `graph_neighbors`

- `src/storage/lance_store/graph.rs::neighbors(node_id)` 加重载或新增 `neighbors_within(node_id, max_hops, valid_at)`：BFS，按 `valid_to IS NULL OR valid_to > valid_at` 过滤
- `GraphNeighborsArgs` 加 `max_hops: Option<u32>`（默认 1，cap 3）和 `as_of: Option<String>`
- HTTP `GET /graph/neighbors/{id}` 加 `?max_hops=N&as_of=...`
- MCP description 写明"BFS, dedupe, exclude expired edges"

### 4.2 `kg_timeline(entity)`

- 新 SQL：`SELECT * FROM graph_edges WHERE from_node_id=? OR to_node_id=? ORDER BY valid_from ASC`
- 返回 `[{predicate, other_node, valid_from, valid_to, active}, ...]`
- service + http + mcp 串

### 4.3 `kg_invalidate(subj, pred, obj, ended_at)`

- 新 SQL：`UPDATE graph_edges SET valid_to = ?ended WHERE from_node_id=?subj AND predicate=?pred AND to_node_id=?obj AND valid_to IS NULL`
- 复用现有 `close_edges_for_memory` 的关边语义，但允许 caller 显式指定 `(subj, pred, obj)` 三元组
- MCP description 强调"幂等"——已 closed 的边不再二次关

### 4.4 文档漂移修复

- CLAUDE.md 图层段："`neighbors_at(node, ts)` supports point-in-time lookups"——要么补实现（即 4.1 里的 `as_of` 参数），要么把那句话改成"point-in-time 通过 `graph_neighbors` 的 `as_of` 参数实现"
- 否则下一波读者会被 grep 不到的 API 名字坑

---

## 5. 时间戳与维护

- 本篇生成时间：**2026-05-12**
- 上一次比较：v1，~2026-05-06（per `mempalace-diff.md` §15 snapshot 日期）
- 维护建议：完成 #16–#20 任一项后，回 §3 的表格标 ✅ 并写 commit hash；新增 MemPalace 上游工具时回 §2 对应小节加一行
