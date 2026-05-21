## MemPalace × mem 对照（v3 — 2026-05-20 module / CLI 表面层补审）

> 本篇承接 [`mempalace-diff-v2.md`](./mempalace-diff-v2.md)。v2 的结论是 MCP 工具表面层 #16–#28 已全部 ✅，剩下都是"有意保留的形态/哲学差异"。**本篇不否认那个结论**——重新核过 mempalace HEAD（commit `de7801e`，2026-04-27），`mempalace/mcp_server.py` 仍是 29 个 MCP 工具，一条没多。
>
> 但 v2 的比较粒度止步于 **MCP 表面**；它没有逐个对照两边的 **module / CLI 表面**。本篇做了那个事：把 mempalace 当前 14 个 CLI 子命令 + 一批非 MCP-暴露的 module（`fact_checker.py` / `dedup.py` / `corpus_origin.py` / `onboarding.py` 等）逐一映射到 mem 的对应物，结果发现 5 处**值得做的缺口**——既不属于 §15.4（已论证拒绝）也不在 v1 §8 / v2 §3 路线图里。
>
> 维护原则同 v1/v2：本篇与代码不一致时**以代码为权威**；落地一项后回到对应表格更新状态（✅ done / 🚧 in progress）。

---

## 0. 方法论

| 步骤 | 来源 |
|---|---|
| 列 mempalace 当前 CLI 子命令 | `grep '^def cmd_' /root/workspace/master/mempalace/mempalace/cli.py` |
| 列 mempalace 非 MCP module | `ls /root/workspace/master/mempalace/mempalace/*.py` + 各 module docstring |
| 列 mem 当前 CLI 子命令 | `ls src/cli/*.rs` + `src/main.rs` 的 clap 子命令 |
| 列 mem MCP 工具 | v2 §2 已对齐，不重复 |
| 分类 | (A) 已等价吸收 (B) 部分覆盖 (C) 真正缺口 (D) §15.4 / v2 §2.8 已拒绝 |

MCP 表面层的对照见 v2，本篇不重复。

---

## 1. 一句话结论

> **MCP 表面层无新缺口；module / CLI 层有 5 处值得补**——集中在 **数据卫生**（fact_check / 近似去重）和 **首跑体验**（`mem init` / per-session cursor / 外发 provider 警告）两块。所有数据 schema 都已就位，缺的是把现有 entity registry / KG / embedding 能力封装成对外 API 或独立 CLI。
>
> 不补：spellcheck（违反 Verbatim §7）、`llm_refine`（§15.4 已论证）、`mem repair`（随 usearch sidecar 一起删，Lance native ANN 没这个 failure mode）。

---

## 2. CLI 表面对照

mempalace HEAD 14 个 CLI 子命令；mem HEAD 5 个（`serve` / `mcp` / `mine` / `wake-up` / `feedback`）。

| mempalace | mem 对应 | 映射强度 | 备注 |
|---|---|---|---|
| `mempalace init` | — | ❌ | **缺**：mode-based 默认 wing taxonomy（`code` / `personal` / `research`）+ 零配置首跑骨架 |
| `mempalace mine` | `mem mine` | ✅ | mem 是 dual-sink（写 capsule + transcript 归档），mempalace 只写 drawer |
| `mempalace sweep` | (`mem mine` 已 per-block) | ⚠️ | 部分覆盖：mem mine 已是 block 粒度，缺**per-session cursor resume**（mempalace `sweeper.py` 按 session_id 跳过 already-mined messages，规模上去后性能差异显著） |
| `mempalace search` | (无独立 CLI) | ⚠️ | mem 通过 MCP `capability_capsule_search` 提供；缺独立 CLI 供脚本/debug 用 |
| `mempalace wakeup` | `mem wake-up` | ✅ | |
| `mempalace split` | — | ❌ | 缺：split mega transcript files（niche——只在 import 外部 mega-dump 时有用）|
| `mempalace migrate` | — | ❌ | 缺：schema migration runner（暂不需要——schema 在源码里 inline 跟 record_batch builder 同步更新；只有跨次 break 时才用得到） |
| `mempalace status` | `mem_health` MCP | ⚠️ | mem 通过 MCP 提供（v2 #28），缺独立 CLI |
| `mempalace repair_status` / `repair` | — | 🚫 | **不做**：mem 把 usearch sidecar 替换成 Lance native ANN，HNSW capacity divergence / blob-seq marker 这些 failure mode 不复存在 |
| `mempalace hook` | (hooks 是 bash shell 脚本) | ✅ | 两边都走 hook，实现形态不同 |
| `mempalace instructions` | (SKILL.md / wake-up payload) | ✅ | mem 通过 `mem wake-up` 注入 SessionStart |
| `mempalace mcp` | (README 写明) | ⚠️ | mem 缺一个"打印怎么接 MCP" 的辅助 CLI——README 章节代偿，但 onboarding 不如一行 CLI 直观 |
| `mempalace compress <id>` | — | ❌ | 缺：调试用 standalone compress（compress.rs 现在只在 retrieve pipeline 里跑）|

---

## 3. Module 级能力对照（mempalace 有 / mem 没有）

> 排除 mempalace 内部的 `palace.py` / `palace_graph.py` / `searcher.py` 等"换皮等价物"——mem 都有对应实现（`storage/lance_store/` + `storage/duckdb_query/` + `pipeline/retrieve.rs`）。下面只列**功能性差异**。

| mempalace module | 做什么 | mem 状态 | 值得做？ |
|---|---|---|---|
| **`fact_checker.py`** | ingest 前过一遍 entity registry + KG，检查 (a) `similar_name` typo（"Alic" vs 已注册的 "Alice"）(b) `relationship_mismatch`（声明 "X manages Y" 但 KG 里是 Y manages X）(c) 与已知 fact 冲突；返回 warning 不阻塞 | ❌ | **建议做**（→ #29）——entity registry 和 KG 都已就位，是薄薄一层，给 review-flow 当 PR-bot 用 |
| **`dedup.py`**（近似去重） | cosine similarity 找同 `source_file` 的近似 capsule，保留 最长最富信息的一行，其他归档 | partial | mem 有 `content_hash` exact-dedup + `idempotency_key`，**没有近似扫**。**建议做**（→ #30）——cron 或 `POST /admin/dedup_sweep` |
| **`corpus_origin.py`** | 探测 corpus 是不是 AI 对话 + 哪个平台 + agent persona names（解 "my three sons in Claude = three AI instances vs three biological children" 这种歧义） | ❌ | **暂不做**——niche，mem mine 默认假定 Claude Code transcript；等接非 Claude 源再说 |
| **`general_extractor.py`** | regex 5 类自动分类（decision / preference / milestone / problem / emotional），在 mine 路径里跑 | ❌ | **谨慎**——和 §15.4 "caller 决定 type" 原则有张力；若做应限于 mine 路径不污染 ingest API。低优先 |
| **`onboarding.py`** | mode-based 默认 wing taxonomies（`code` / `personal` / `research`），`mempalace.yaml` 初始化 | ❌ | **建议做**（→ #31）——和 #31 `mem init` 一起 |
| **`spellcheck.py`** | 写入前拼写纠正，保留技术词 / CamelCase / 实体名 / URL | ❌ | 🚫 **不做**——违反 Verbatim §7 |
| **`llm_refine.py`** | opt-in LLM 二次精排 entity 候选（PERSON/PROJECT/TOPIC/COMMON_WORD/AMBIGUOUS）| ❌ | 🚫 **不做**——§15.4 已论证（服务端不烧 LLM） |
| **`dialect.py`**（AAAK 输出格式）| 把抽取结果序列化成 AAAK 紧凑结构 | ❌ | 🚫 **不做**——AAAK 整体在 §15.4 已拒绝 |
| **隐私守护**（commits `a0b7ba0` Tailscale CGNAT / `4400734` external-LLM warn）| 启动 / 配置时警告 embedding provider 外发，识别 Tailscale CGNAT (`100.64.0.0/10`) 为本地 | ❌ | **建议做**（→ #33）——mem 现在 `EmbeddingProvider::OpenAI` 静默上线，应该给一次启动警告 |
| `sweeper.py` | message-granular fallback miner + per-session cursor resume | partial | mem mine 已经 block 粒度，但缺 cursor 持久化。**建议做**（→ #32） |
| `split_mega_files.py` | 切分超大单文件 session | ❌ | 暂不做——niche |
| `repair.py` | rebuild vector index from metadata，HNSW capacity divergence detection | ❌ | 🚫 **不做**——随 usearch sidecar 一起删，Lance native ANN 没这个 failure mode |
| `migrate.py` | ChromaDB 跨版本 schema 迁移 | ❌ | 暂不做——mem 在 source 里 inline schema，没有跨版本兼容需求 |

---

## 4. 推荐路线（#29–#33，沿用 v2 §3 编号风格）

> 编号继续 v2 §3（最后是 #28），新条目从 #29 起。commit 引用格式：`(closes mempalace-diff-v3 #N)`。

| # | 题目 | 改动面 | 工作量估 | 优先级 |
|---|---|---|---|---|
| **#29** ✅ | **`fact_check` API + MCP**：`POST /fact_check` body `{tenant, content, topics, relationships}` → `{similar_names, relationship_conflicts, kg_contradictions}`；read-only 不写库；MCP wrapper `capability_capsule_fact_check`。复用 `EntityRegistry::resolve_or_create` 的 normalize 路径 + Levenshtein ≤ 2 找 typo（token len ≥ 4 floor 防 trivial 撞）；扫 `graph_edges` 找 (a) 方向反转 same-predicate (b) 同 (S,P,*) 不同 object (c) 重述已 closed 的 (S,P,O)。**`relationships` 由 caller 传**——mem 无 LLM 抽取，与 §15.4 一致 | 新 `service/fact_check_service.rs`（FactCheckService + FactCheckError）+ `http/fact_check.rs` + `error.rs` 加 `From<GraphError>` + `mcp::server` 加 `FactCheckArgs` + `capability_capsule_fact_check` tool；9 个 integration tests | M（实际 ~3h） | **P1** — 9/9 tests green, fmt + clippy clean |
| **#30** ✅ | **近似去重 worker**：`src/worker/dedup_worker.rs::sweep_once` —— 按 `(source_agent, project, repo)` 分组活跃 capsule，组内拉 embedding 向量做 pairwise cosine + union-find 聚类，cosine ≥ `threshold` 的聚成一簇，保留 `len(content)` 最大（tie-break 最早 `created_at`）的一条，其余通过 `apply_feedback(FeedbackKind::Incorrect)` 走软删；`dry_run=true` 只报告候选 id 不写。`DedupSettings` **默认 OFF**（destructive），`MEM_DEDUP_ENABLED=1` 开；`MEM_DEDUP_INTERVAL_SECS` / `MEM_DEDUP_THRESHOLD` / `MEM_DEDUP_SCAN_LIMIT` 调参 | 新 `EmbeddingVectorStore::get_capability_capsule_embedding_vector` trait 方法 + Lance impl（读 `FixedSizeListArray`）+ `config::DedupSettings` + `worker/dedup_worker.rs` + `app.rs` 条件 spawn + 5 个 integration tests | M（实际 ~2h） | P1 — 5/5 tests green, fmt + clippy clean |
| **#31** ✅ | **`mem init` CLI**：`mem init [--mode code\|personal\|research] [--path PATH] [--force]` 在 `<path>/.mem/` 下写三个文件：`config.env`（`MEM_DB_PATH` / `BIND_ADDR` / `EMBEDDING_PROVIDER=fake` 等保守 env defaults，含被注释的 `MEM_DEDUP_ENABLED` 提示）、`taxonomy.toml`（mode 对应的 starter `projects` / `repos` 列表——code/personal/research 三套，纯文档不参与运行时）、`README.md`（下一步引导）。Refuse-overwrite-without-force（exit code 2）。**走 `.env` 路线不引入 TOML 配置加载**——mem 现在全部走 env vars，新增 TOML 解析层会是另一个 surface，文件只是声明而非运行时输入 | 新 `src/cli/init.rs` + `cli/mod.rs` 注册 + `main.rs` 子命令；6 个单元测试 | S（实际 ~1h） | P1 — 6/6 tests green, fmt + clippy clean |
| **#32** | **`mem mine` per-session cursor**：新 lance 表 `mine_cursors(session_id, last_mined_ts, updated_at)`；`mine` 每次开扫前查 cursor，跳过 `timestamp < cursor` 的 block；写完更新 cursor。**幂等性已由 server-side dedup 兜底**，cursor 是性能优化不是正确性 | 1 个新 lance 表 + lance_store/duckdb_query CRUD + `cli/mine.rs` 串通 | S | P2 — 规模上去再做（当前 mine 5 万 block 也只需几秒） |
| **#33** ✅ | **外发 embedding provider 启动警告**：`AppState::from_config` 时如果 `EmbeddingProviderKind::sends_off_machine()` 为 true（OpenAi 命中；Fake / EmbedAnything 不命中）就 `tracing::warn!("embedding provider sends content OFF this machine ...")`。**分类落在 enum method 上**（compiler exhaustive match 强制新 variant 显式选边），不在 caller 字符串拼接里，避免新增 hosted provider 时静默漏出。Suppress via `MEM_PRIVACY_WARN_SUPPRESS=1`。`mem init` config.env 模板的 `EMBEDDING_PROVIDER` 注释也提到 warning + suppression env | `config.rs` 加 `sends_off_machine` + 1 个 unit test + `app.rs` warn block + `cli/init.rs` 注释 | S（实际 ~30min） | ✅ — 1/1 unit test green, fmt + clippy clean |

### 决策点

- **已完成**：#29（fact_check API + MCP）+ #30（dedup worker）+ #31（`mem init` CLI）+ #33（外发 embedding provider 启动警告）—— 9+5+6+1 = 21 个新测试 green。**额外**：ROADMAP incident TODO #3 (multi-process write guard) 也一并落地 (`storage::open_lock` + 5 unit tests)
- **剩**：#32（per-session mining cursor）—— 规模性能优化，未到痛点；可做可不做
- **不做**：spellcheck / llm_refine / dialect / repair / corpus_origin / general_extractor / split_mega_files / migrate（理由见 §3 表的 🚫 标）

---

## 5. 实施清单（#29 详化，给执行者参考）

> #29 是本批最有体量的一项；展开如下。其余 #30–#33 按照"先 service 后 http 后 mcp"标准三层套用即可。

### 5.1 接口形状

```jsonc
// Request
{
  "tenant": "local",
  "content": "Alic Smith manages Project Phoenix as of Q1 2026",
  "topics": ["project-phoenix", "alic"],
  "code_refs": []
}

// Response — read-only, no side effects
{
  "similar_names": [
    { "in_input": "Alic", "matches": [{ "entity_id": "ent_01...", "canonical_name": "Alice", "edit_distance": 1 }] }
  ],
  "relationship_conflicts": [
    { "subject": "alice", "predicate": "manages", "object": "project-phoenix",
      "existing_edge": { "subject": "project-phoenix", "predicate": "managed_by", "object": "alice", "valid_from": "..." },
      "note": "direction mismatch: existing edge has reversed subject/object" }
  ],
  "kg_contradictions": [
    { "claim": "alice manages project-phoenix",
      "existing": { "from": "project-phoenix", "to": "bob", "relation": "managed_by", "valid_to": null },
      "note": "active fact in KG: project-phoenix is currently managed by bob" }
  ]
}
```

### 5.2 落地步骤

1. **`src/service/fact_check_service.rs`** 新模块：
   - `check_similar_names(tenant, content, topics)`：拆词 + `EntityRegistry::resolve_alias` 命中失败的进 Levenshtein ≤ 2 候选；
   - `check_relationship_conflicts(tenant, subject, predicate, object)`：查 `graph_edges` 反向边、已 closed 的同断言；
   - 返回 `FactCheckReport` 结构体（read-only）
2. **`src/http/fact_check.rs`** 新 router：`POST /fact_check`
3. **`src/mcp/server.rs`** 加一个 `capability_capsule_fact_check` tool
4. **测试**：
   - 单元：`tests/fact_check.rs`，至少覆盖 typo 命中、方向反转、冲突 active fact 三类
   - 集成：起 `mem serve`、posts 一组 fixture、断言返回 shape

### 5.3 边界说明

- **不做 LLM**——一切判断走 entity registry + graph_edges 的结构化数据。与 §15.4 一致
- **不阻塞 ingest**——纯 read-only API，caller 自己决定是否要根据 report 改 input 再 ingest
- **Levenshtein 上限 ≤ 2 + len(token) ≥ 4**——避免 "a" ↔ "b" 这种 trivial 匹配淹没结果
- **direction-mismatch 只覆盖 KG 已建过的 predicate 对**——首次出现的 predicate 没法判方向，跳过

---

## 6. 时间戳与维护

- 本篇生成时间：**2026-05-20**
- 上一次比较：v2，2026-05-12
- 检查的 mempalace HEAD：commit `de7801e`，2026-04-27（v2 之后 mempalace 未新增 MCP 工具，仍是 29 个）
- §7 KG 层补审：2026-05-21
- 维护建议：
  1. 完成 #29–#33 任一项后，回 §4 表格标 ✅ 并写 commit hash
  2. 新增 mempalace 上游 module / CLI 时回 §2 / §3 对应表加一行
  3. 当 v1 §15.2/15.3/15.4 三张映射表需要变动时，同时回 ROADMAP.MD 追加新行号
  4. KG 侧（K1–K5）回 §7 表格更新

---

## 7. KG 侧补审（2026-05-21）

> §2-§3 覆盖了 mempalace 的 MCP / CLI / module 表面，但**单独把 KG 层拎出来比**还没做过。本节对照 mempalace 的 `knowledge_graph.py` + `palace_graph.py` + `entity_registry.py` + `entity_detector.py` 四个 KG 模块和 mem 的图层（`storage/graph_store`、`storage/entity_registry`、`pipeline/ingest::extract_graph_edge_drafts`、MCP `capability_capsule_kg_*`），产出 K1-K5 五项优化方向。
>
> 维护原则同 §4：完成一项回到对应行标 ✅ + commit hash。

### 7.1 表面对照（mempalace vs mem）

| mempalace | mem | 状态 |
|---|---|---|
| `triples(subject, predicate, object, valid_from, valid_to, confidence, source_*, adapter_name, extracted_at)` | `graph_edges(from_node_id, to_node_id, relation, valid_from, valid_to)` | mem 缺 **confidence** + **source/adapter provenance** |
| `entities(id, name, type, properties JSON, created_at)` | `entities(id, tenant, canonical_name, kind, created_at)` | mem 缺 **properties JSON** 字段 |
| `add_triple` 自动 dedup on active triple | `sync_memory_edges` / `add_edge_direct` 同形态 | ✅ |
| `invalidate(subj, pred, obj, ended)` | `invalidate_edge` (v2 #16) | ✅ |
| `query_entity(name, as_of, direction)` | `neighbors_within(node, max_hops, as_of)` (v2 #16) | ✅ |
| `query_relationship(predicate, as_of)` | — | ❌ → **K4** |
| `timeline(entity_name)` | `kg_timeline(node_id)` (v2 #16) | ✅ |
| `stats` | `graph_stats` (v2 #17) | ✅ 不同视角（mempalace `rooms_per_wing` vs mem `top_relations`）|
| `build_graph` 从 drawer metadata 推 topology | mem 直接存边为 first-class | 🔄 redesigned — mem 哲学不同 |
| `compute_topic_tunnels(topics_by_wing, min_count)` | `topic_tunnel_worker` (2026-05-21) | ✅ **K2** — `2a964ee` |
| `topic_tunnels_for_wing(wing)` 增量 | 包含在 K2 sweep 中（每次扫全量，简单实现）| ✅ K2 收口 |
| explicit tunnels (caller-curated) | `user_tunnel:*` relation prefix (v2 #20 phase A) | ✅ |
| `traverse(start_room, max_hops)` BFS | `neighbors_within` BFS | ✅ |
| `find_tunnels(wing_a, wing_b)` | `find_tunnels(prefix_a, prefix_b)` (v2 #23) | ✅ |
| `_fuzzy_match` on missing room | — | ❌ → **K5** |
| 图缓存 + TTL + invalidate | — | ⚠️ DuckDB 实时查已经快，边际价值小 |
| `entity_detector.detect_entities` 从 prose 抽取 | — | ⏸️ ROADMAP v1 #20 LATER |
| `learn_from_text` + `wikipedia_lookup` | — | 🚫 不做（offline-first + Verbatim） |
| 模式化 entity seed（`seed(mode, people, projects)`） | `mem init --mode` 只写 taxonomy 不 seed entity | 🚧 partial（K8）|

### 7.2 优化方向（K1–K8）

| K# | 题目 | 工作量 | 状态 |
|---|---|---|---|
| **K1** | edge `confidence` 列 —— caller 可声明 "这条边 0.6 可信"，retrieve `graph_boost` 用它加权 | L（实测 ~1 天：schema 加列 + Lance `add_columns(AllNulls)` 迁移 + GraphEdge 22 处构造位 + 7 处 record_batch helper + DuckDB SELECT 投影 + 测试矩阵）| ❌ 未做。**K2 已用 relation prefix `user_tunnel:topic:%` 替代部分价值**；真正想做 K1 时需要单独排期一次 spec-then-implement session |
| **K2** ✅ | `compute_topic_tunnels` 等价 worker —— 按 project 分组扫 active capsule，topic overlap >= `min_count` 时自动建 `user_tunnel:topic:<X>` 边；幂等via `add_edge_direct` | M（实际 ~3h）| ✅ `2a964ee` —— 6/6 unit tests green；默认 OFF + `MEM_TOPIC_TUNNEL_ENABLED=1` |
| **K3** | edge `extractor` / `source_adapter` 字段 —— 标记每条边由哪条代码路径产生（`tagged_extractor` / `file_ref_extractor` / `caller_supplied` / `topic_tunnel_worker`）| S（~2h）| ❌ 未做。**当前用 relation 前缀间接区分**（K2 capsule `mem_019e497e` 详）；正式 column 等 K1 一起做 |
| **K4** | `kg_query_predicate(predicate, as_of)` MCP —— 列所有 `predicate=X` 的活跃/历史边 | S（~1h）| ❌ 未做 |
| **K5** | fuzzy match on `graph_neighbors` —— node_id 不存在时返回 `{neighbors: [], suggestions: [...]}`（复用 v3 #29 fact_check 的 Levenshtein）| S（~1h）| ❌ 未做 |
| **K6** | `entities.properties` JSON 字段（caller 自由 metadata）| M | ⏸️ YAGNI，无具体场景 |
| **K7** | 图缓存（TTL + invalidation）| S | ⏸️ DuckDB 实时查已经快 |
| **K8** | `mem init --mode` 写 starter entities（不止 taxonomy）| S | ⏸️ 轻量 UX 改善 |

### 7.3 决策点

- **已完成**：K2（topic-tunnel worker auto-derived cross-project edges）
- **下一波可选**：K4 + K5 一组（~2h，纯 MCP 补齐，不动 schema）
- **正式 K1+K3 排期**：单独一次 session，先写 spec（schema migration 策略 + caller 更新清单 + 测试矩阵），再实现
- **不做**：K6/K7/K8 暂搁；prose extraction + Wikipedia 已在 §15.4 / v1 #20 论证过
