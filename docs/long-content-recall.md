# ③ 长内容召回 — chunked embeddings (v1 — 2026-06-01)

> 这是 mem 的**第三条平行工作线**，与前两条正交：
> - **①** MemPalace 对齐 —— [`mempalace-diff.md`](./mempalace-diff.md) (v1–v4) + 执行面板 [`ROADMAP.MD`](./ROADMAP.MD)（#1–#36 / K1–K12）
> - **②** Backend 存储抽象 —— [`backend-coupling.md`](./backend-coupling.md)（QW-1…6 / Phase 0–5 / LT-1…6）
> - **③** 长内容召回（本篇）—— 把超过 embedder 上下文窗的长内容切成多段嵌入，消除"尾部静默截断、语义召不回"这个隐式正确性边界
>
> 起因不是上游对照，也不是 storage 重构，而是一条实测正确性 bug：`memories.content` / transcript block `content` 一旦超过 embedder 上下文窗（Qwen3-Embedding ~32k tokens），尾部被 embedder **静默截断**，那部分内容用语义检索**永远召不回**（BM25 仍按词覆盖，所以症状是"换个说法就搜不到了"）。本篇登记这条线的设计与落地状态。
>
> **维护原则**同 ① / ②：本篇与代码不一致时**以代码为权威**；落地一个 phase 后回到 §4 表格更新状态（✅ done / 🚧 in progress）+ commit hash（格式 `… (refs long-content-recall ③ pN)`）。

---

## 0. 基线

| 项 | 值 |
|---|---|
| 起始 commit | `f6edb1c`（③ phase 1，2026-06-01） |
| 当前 commit | `108d9fe`（③ transcript parity，2026-06-01） |
| 切分原语 | `src/pipeline/chunk.rs::chunk_text(text, max_tokens, overlap)` |
| tokenizer | `o200k_base`（`tiktoken_rs`，与 `compress.rs` 同源；作为 embedder 限额的**保守代理**——边界与 Qwen3 tokenizer 不同，但 chunk 远小于任何 embedder 限额，所以代理安全） |
| 默认窗口 | `DEFAULT_CHUNK_TOKENS = 2000` |
| 默认重叠 | `DEFAULT_CHUNK_OVERLAP = 200`（跨窗口边界的事实在至少一个 chunk 里完整出现） |

---

## 1. 一句话结论

> mem 把 capsule 的 `summary + content`、transcript block 的 `content` 作为**单个向量**嵌入；长内容尾部被 embedder 静默截断 → 语义召回丢失。修法是**多向量-单实体**：把内容切成重叠 token 窗口，**一段一行 embedding**（共享实体 id），检索时 `GROUP BY <id> MIN(_distance)` 把多 chunk 命中塌缩回一条实体、取最佳 chunk 距离。
>
> **关键性质**：① **无 schema 变更**——chunk 只是同 id 的额外行，靠读时 GROUP BY 去重;② **短内容字节级不变**——`<= DEFAULT_CHUNK_TOKENS` 返回单 chunk = 原文,即今天的单行行为;③ **两条管线都要做**——memories 与 transcripts 是零共享状态的平行管线，修一条不会自动修另一条。

---

## 2. 问题：尾部静默截断

- **写**：embedding worker 把 `summary + "\n" + content`（capsule）/ `content`（transcript block）整体送 embedder，得 1 个向量，写 1 行。
- **截断**：内容 token 数超过 embedder 上下文窗时，embedder **静默**丢弃尾部——不报错、不警告。
- **症状**：长内容的尾部**没有任何向量代表**，语义检索按 head 命中或干脆不命中；BM25 仍按词覆盖，所以表现为"长记忆/长会话块换个语义说法就搜不到"，且无任何信号提示发生了截断。
- **层次**（见 [`mempalace-diff.md`](./mempalace-diff.md) §8 两轴）：这是 🔍 索引/检索层的**隐式正确性边界**，不是 📦 存储问题——`content` 本身一直 verbatim 完整存着，丢的只是它的**向量表示**。

---

## 3. 设计：多向量-单实体 + 读时聚合

三个零件，capsule 与 transcript 两条管线各一套：

1. **切分原语**（`pipeline/chunk.rs`）：`chunk_text` 把文本切成步进 `max_tokens - overlap` 的重叠窗口；`<= max_tokens` 直接返回 `[text]`（逐字，不过 tokenizer round-trip）；`overlap` 钳到 `max_tokens - 1` 保证前进；多字节字符不会被 BPE token 边界劈开。

2. **写 N 行**（`upsert_*_embedding_chunks`）：先按实体 id **删一次**旧行，再**一段一行**插入（所有行共享实体 id；Lance 无 PK，靠读时去重）。全 chunk 成功才落盘——任一 chunk 嵌入失败/维度不符则整 job 重排，实体**绝不**留下半套 chunk。

3. **读时聚合**（search 的 ANN 分支）：`lance_vector_search` 对 N 个 chunk 行各返一行；外层 `GROUP BY <id>, MIN(_distance)`（最佳匹配 chunk）在 JOIN 前塌缩成一条实体、取最佳 chunk 距离。对单 embedding 实体是 no-op（一行的 GROUP BY 不变）。ANN oversample ×2→×4，因 N 个 chunk 行塌缩后 distinct 实体变少。

**为什么不加 schema 列**：chunk 是同 id 的额外行，写时不需要 `chunk_index` 列（不读单 chunk，只读"实体的最佳 chunk 距离"），读时 GROUP BY 已收口。加列反而要迁移 + 改 record_batch + 改投影——多向量-单实体的语义用"多行 + 读时聚合"表达更省。

**两条管线对照**：

| | memories（capsule） | transcripts（block） |
|---|---|---|
| 嵌入源 | `summary + "\n" + content` | `content`（无 summary 前缀） |
| 写 N 行 | `upsert_capability_capsule_embedding_chunks` | `upsert_conversation_message_embedding_chunks` |
| 读聚合 | `hybrid_candidates` 的 `vec` CTE | `semantic_search_transcripts` |
| worker | `worker/embedding_worker.rs`（跨 job 批量 + 按 job regroup） | `worker/transcript_embedding_worker.rs`（单 job/tick + 按 chunk regroup） |
| 去重键 | `capability_capsule_id` | `message_block_id` |

---

## 4. Phase 表

| phase | 题目 | 改动面 | 状态 |
|---|---|---|---|
| **③ p1** | 切分原语 | `pipeline/chunk.rs::chunk_text` + 常量 + TDD（短-逐字 / 长-切分保 head+tail / overlap-钳位终止 / 连续重叠） | ✅ `f6edb1c` |
| **③ p2-search**（capsule） | ANN 命中聚合到 capsule 级 | `hybrid_candidates` 的 `vec` CTE 加 `GROUP BY capability_capsule_id, MIN(_distance)`；先于 p2-worker 落地，避免多行双计 | ✅ `53b2b0c` |
| **③ p2-worker**（capsule） | worker 切 N 段写 N 行 | `upsert_capability_capsule_embedding_chunks`（trait + Store + LanceStore）+ `embedding_worker` 切分/批量/regroup + oversample ×4 | ✅ `b265858` |
| **③ transcript parity** | transcript 管线镜像 | `upsert_conversation_message_embedding_chunks` + `transcript_embedding_worker` 切分 + `semantic_search_transcripts` GROUP BY + oversample ×2→×4 | ✅ `108d9fe` |

> 验证：每个 phase 自带 TDD（RED→GREEN）。关键 dedup 测试：capsule 侧 `duckdb_query_hybrid_candidates_dedups_multi_chunk_capsule`、transcript 侧 `semantic_search_transcripts_dedups_multi_chunk_message`——head 查询 AND tail 查询都必须**恰好命中一次**（RED 下无 GROUP BY 返回两次）。两次落地全量套件 green。

---

## 5. 剩余 / 不做

- **存量回填（运维，未自动化）**：chunking 只对**新入队**的 embedding job 生效。③ 之前已嵌入的长 capsule / 长 block 仍是"整条截断"的单行，要靠 `POST /embeddings/rebuild`（`embeddings_rebuild` MCP / `capability_capsule_service::rebuild_embeddings`）按 id 重嵌才会切片。**没有**自动 backfill sweep——需要时写一次性脚本枚举超长内容的实体重嵌。
- **量化基线缺失** ⚠️：③（连同 K9/K10）这些动检索/排序的改动**没有量化召回评估**——`#14/#15` 两套 bench 在 `4df527b` 随旧 usearch 删除，`bench-fixtures/` 已空。要证明 chunking 真的提升长内容召回，需重建 harness（`src/pipeline/eval_metrics.rs` 的 recall@k / ndcg / mrr 仍可复用）。见 [`ROADMAP.MD`](./ROADMAP.MD) §下一阶段。
- **不做**：
  - **chunk-aware compress**——输出层 `pipeline/compress.rs` 仍在 verbatim `content` 上按 token 预算压缩，与嵌入切分无关，不动。
  - **`chunk_index` 列 / 单 chunk 检索**——见 §3"为什么不加 schema 列"。需要"定位命中在长内容的哪一段"时再议。
  - **可调窗口/重叠的 env 旋钮**——`DEFAULT_CHUNK_TOKENS` / `OVERLAP` 是常量，远低于任何 embedder 限额，无实测需要前不引入配置面。

---

## 6. 时间戳与维护

- **创建**：2026-06-01。基线 commit `f6edb1c`…`108d9fe`。
- **维护建议**：
  1. 新增 phase 后回 §4 表标 ✅ + commit hash（`… (refs long-content-recall ③ pN)`）。
  2. 落地"存量回填脚本"或"召回 bench 重建"后，把 §5 对应条目转成 ✅ + 指向实现。
  3. ③ 与 ① / ② 不互相阻塞；commit close 引用各用各的（`refs long-content-recall ③ …` vs `closes mempalace-diff-v4 K…` vs `closes backend-coupling §6 Phase N`）。
