# H4 Skill 结晶 —— 从占位符到可复用流程

**日期**：2026-08-01
**状态**：设计已定稿，待实现
**路线图**：H4（`docs/oss-memory-diff.md` §9）、I5 收口行；`docs/evolution-worker.md` §6.2 Phase 2

---

## 1. 问题

H4 在 `docs/oss-memory-diff.md:320` 标 ✅，但落地的只有**检测**：

- `detect_merge` 里的 `is_procedural_sibling_cluster`（零 LLM 结构判定：`fact_anchors` = commit-sha 形 token ∪ `code_refs`，成员两两近不相交）识别出「同一流程的 N 次执行」；
- 命中且成员数 ≥ `generalize_min_n` 时，`execute_generalize(workflow=true)` 铸一条 Workflow 型 `PendingConfirmation` 占位胶囊（tag `evolution:workflow`，`generalizes` 血缘指向各源）。

**真正的结晶没做**。`src/evolution/synthesis.rs` 文件头写得很直白：`ReviewSynthesisBackend` "performs NO generation"，占位符 content 是一段固定英文评审指令 + `Shared topics:` + `id — summary` 列表，真正的流程要人用 `review_edit_accept` 手写。

### 1.1 线上现状（2026-08-01 核实）

存在一条真实占位符 `mem_019fa7fc-bcdf-7f50-a445-0d20e850722f`：

- `status: pending_confirmation`，type `workflow`，tag `evolution:workflow`，`source_agent: evolution_worker`
- `project: xmbox-rs`，topics `aibox-nvr / java-to-rust / migration`
- `evidence` = 5 条 NVR-APP Java→Rust 迁移踩点胶囊（commit `282b74c` / `be2e327` / `4465b9a` / `6bbfee5` / `24e5cb9`），5 条 `generalizes` 边齐全
- 创建后约 6 天无人评审
- `embedding.status = "none"` —— 无向量，只能靠 BM25 命中

### 1.2 附带缺陷：占位符污染 `suggested_workflow` 槽

`capability_capsule_search` 会把这条占位符当作**建议流程**发给 agent，返回的 `suggested_workflow.steps` 就是评审指令原文：

```
["EVOLUTION PROPOSAL — workflow generalize (sibling executions → procedure)",
 "Review task: the source capsules below are N executions of the SAME recurring procedure...",
 "Shared topics: aibox-nvr, java-to-rust, migration"]
```

链路已核实：

- `src/pipeline/retrieve.rs:699-704` 对 `Provisional | PendingConfirmation` 只 `score -= 4`，**不排除**；
- `src/pipeline/compress.rs:69` 取排名最前的 Workflow 型胶囊填 `suggested_workflow` 槽，`compress.rs:100-102` 按 type 路由。

该 scope 下它是唯一的 Workflow 胶囊，扣 4 分照样中选。**给人看的评审工单被当成给 agent 用的流程发了出去。** 这与结晶是同一件事的两面：占位符停在半路，既没变成可用流程，又占着可用流程的位置。

---

## 2. 目标与非目标

**目标**

1. 把 `evolution:workflow` 占位符变成一条真正可复用的步骤化 Workflow 胶囊。
2. 不破坏 mem 的两条硬纪律：常驻进程 LLM-free；生成产物永不未经人眼直接进活跃池。
3. 修掉 §1.2 的占位符污染。

**非目标**

- 不导出 `SKILL.md` 文件。产物就是 Workflow 胶囊，消费走已有的 `suggested_workflow` 槽（评估结论：文件形态要额外回答「写哪个仓 / 胶囊改了文件怎么跟 / 人改了文件回写吗 / 拒绝后谁删」四个问题，mem 是服务不是文件管理器）。
- 不点亮 `MEM_EVOLUTION_SYNTHESIS=local|api`。`src/config.rs:1275-1281` 对这两档的解析期拒绝**保持不变**——那会让常驻 worker 在 sweep 里调模型，正面违反 `docs/evolution-worker.md:69` 原则 5。
- 不做跨候选合并提炼（理由见 §4.3）。

---

## 3. 架构

新增一个一次性 CLI 子命令 `mem crystallize`，形态照抄 O7(c) 的 `src/cli/llm_extract.rs` —— mem 里唯一已有的生成式 LLM lane，且是"fail-safe by construction"的既有先例。

**关键约束：CLI 纯 HTTP 客户端，不直连 store。** 依据是 E2/E3 的教训（胶囊 `mem_019f20ad`）：*"mem serve 是 Lance 数据集的单写者，独立 CLI 进程直连 store 会跟在跑的 serve 打架。任何『运维一击』类需求在这个架构下都应该走 HTTP 面。"*

所需 HTTP 面**全部已存在，零新端点**：

| 步骤 | 端点 | 已存在 |
|---|---|---|
| 拉占位符 | `GET /reviews/pending` | ✅ `src/http/review.rs:13` |
| 读源胶囊逐字内容 | `GET /capability_capsules/{id}` | ✅ |
| 回写 | `POST /reviews/pending/edit_accept` | ✅ `src/http/review.rs:16-19` |

### 3.1 数据流

```
mem crystallize [--candidate <id>] [--accept]
  │
  ├─ GET /reviews/pending          → 过滤 tags 含 "evolution:workflow"
  │
  ├─ 读占位符 evidence[] 的源 id
  ├─ GET /capability_capsules/{id} ×N
  │     → 取 content（逐字），不是 summary
  │        ← 守 verbatim 规则：占位符本身不拷源 content，
  │          结晶时才按需拉全文，拉完只进 prompt 不落库
  │
  ├─ POST {LLM_API_BASE}/chat/completions   ← llm_entry 网关
  │     prompt: 「以下 N 段是同一流程的 N 次执行（各自引用不同
  │              commit/文件）。提炼成一条可复用的步骤化流程。」
  │
  ├─ 默认（无 --accept）：打印生成结果，退出，零写入
  │
  └─ --accept：POST /reviews/pending/edit_accept
        → 铸新 Workflow 胶囊（Active），血缘从占位符 evidence 重写到 successor
```

### 3.2 三重 fail-safe

照抄 O7(c) 的三闸结构，任何一闸不过都退化成"什么也没发生"：

1. **子命令不跑 = 零行为。** 不是 worker，没有定时触发，没有默认执行路径。
2. **`LlmExtractConfig::from_env` 返回 `None`**（`LLM_API_BASE` / `LLM_MODEL` 未设）→ 打印提示并退出，不做任何网络调用。
3. **生成失败吞掉错误** → 占位符原样留在评审队列，评审路径完全不变。绝不因为 LLM 挂了而破坏既有状态。

reqwest client 沿用 O7(c) 的 `.no_proxy()`（内网网关调用不能走环境里的 `HTTP(S)_PROXY`，否则 502）。`LLM_API_KEY` 对内网网关留空（不发 `Authorization`）。

### 3.3 Active 闸

§6.2 写死「自动后端产物强制过 `PendingConfirmation`，永不直接 Active」。`edit_accept` 按定义转 Active，所以闸门放在 CLI 上：

- **默认 dry-run**：只打印生成的流程，不写库。契合 mem 到处都是 dry-run 默认的先例（`POST /reviews/evolution {dry_run}`、`idle_archive`、`auto_promote`）。
- **`--accept` 才提交**：人看过生成结果后显式加旗标。

人仍在环上——纪律的真实意图是"生成内容在成为权威之前必须过人眼"，审阅点从评审队列挪到 CLI，意图不变，且零新机制。

---

## 4. 细节决策

### 4.1 prompt 输入用 content 不用 summary

占位符里只有 `id — summary`（守 verbatim 规则，故意不拷源 content）。但 summary 是 80 字截断的索引提示，不足以提炼流程。CLI 结晶时按 id 拉全文，**只进 prompt、不落库**——verbatim 规则约束的是存储，不约束读取。

### 4.2 输出落在哪个字段

`edit_accept` 的 patch 写 `content` = 生成的步骤化流程，`summary` = 一句话流程名。二者必须不同（`pipeline/ingest.rs` 强制 caller summary ≠ content）。

### 4.3 多候选：一候选一流程，簇长大走 supersede

不做跨候选合并。上游 `detect_merge` 已在 0.88 细簇上聚过，一个姊妹簇只出一个占位符，跨候选再合并是重复劳动。

真正会复发的是**同一流程的簇长大**：迁移继续 → 第 6、7 条踩点进来 → 新 member 集 → `executed`-history 抑制不匹配（member_ids 变了）→ 新候选、新占位符。

处理：CLI 结晶前查是否存在已 accept 的 Workflow 胶囊，其 `evidence` 与当前候选 members 的重叠率超过阈值。有则把新流程 **supersede** 到旧的上（走 mem 已有的版本链，检索自动排旧版），而不是并列两条。零新机制。

重叠率定义取 **`|A ∩ B| / min(|A|, |B|)`，阈值 0.5** —— 与 `is_procedural_sibling_cluster` 判定锚点不相交时用的 `overlap/min ≤ 0.2` 同一个度量口径，保持全模块一致。用 `min` 而非并集（Jaccard）是因为簇长大的典型形态是「旧 5 条 ⊂ 新 7 条」：Jaccard = 5/7 ≈ 0.71 尚可，但若长到 12 条则 Jaccard = 5/12 ≈ 0.42 会跌破阈值、错判成两条独立流程；`min` 口径下始终是 5/5 = 1.0，正确识别为同一流程的延续。

### 4.4 污染修复（§1.2）

`src/pipeline/compress.rs` 填 `suggested_workflow` 槽时跳过非 `Active` 状态的胶囊。

理由：`suggested_workflow` 的语义是"给 agent 照着做的流程"，一条待审提案不具备这个权威性。评审面走 `/reviews/pending`，不依赖召回，所以排除它不影响任何评审动作。

范围严格限定在 Workflow 槽——`relevant_facts` / `directives` 段的 PendingConfirmation 行为**不动**（那里 -4 惩罚的降权语义是对的，待审事实作为弱信号出现是合理的）。

---

## 5. 测试

集成测试放 `tests/`（本仓无 colocated `*_test.rs` 约定）。用假 provider 注入确定性生成结果，形态参照 `MEM_RERANK_PROVIDER=fake` 的先例。

| # | 锁什么 | 断言 |
|---|---|---|
| 1 | 缺 env 零行为 | `LLM_API_BASE` 未设 → 退出码 0、占位符字节级不变、无网络调用 |
| 2 | 生成失败不破坏状态 | provider 返回错误 → 占位符 status 仍 `PendingConfirmation`、content 不变 |
| 3 | dry-run 不写库 | 无 `--accept` → 打印非空、占位符不变 |
| 4 | 成功路径 | `--accept` → 产出 Workflow 胶囊 status `Active`；`generalizes` 血缘从占位符重写到 successor（E2/E3 已知行为：`edit_and_accept_pending` 铸新 id，边从占位符 `evidence` 重写） |
| 5 | 簇长大 supersede | 已存在重叠 > 50% 的已结晶 Workflow → 新胶囊 `supersedes_capability_capsule_id` 指向它，而非并列 |
| 6 | 污染修复 | 只有 PendingConfirmation Workflow 时，`search` 的 `suggested_workflow` 为 `None`；转 Active 后正常填充 |

单元测试内联在源文件底部 `#[cfg(test)] mod tests`：prompt 组装、重叠率计算、候选过滤。

---

## 6. 验收

- `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` 双绿（CI 强制，含 `tests/`）。
- 上述 6 项集成测试全绿。
- 拿线上真实候选 `mem_019fa7fc-bcdf-7f50-a445-0d20e850722f` 跑一次 dry-run，人工确认生成的流程确实概括了 5 次迁移执行的共同步骤。

---

## 7. 不做的事（显式记录，防后人重提）

| 提法 | 为什么不做 |
|---|---|
| 点亮 `MEM_EVOLUTION_SYNTHESIS=local\|api` | 会让常驻 worker 在 sweep 里调模型，违反 `docs/evolution-worker.md:69` 原则 5。`config.rs` 的解析期拒绝是**特性不是缺陷**，保持原样。 |
| 导出 `SKILL.md` | 见 §2 非目标。若将来真要，胶囊是真相源、文件是可重建的投影，另开一轮设计。 |
| 让 worker 自动结晶 | 同 ①。结晶必须由显式调用触发。 |
| 生成后直接转 Active | 违反 §6.2 的 PendingConfirmation 强制闸，且 LLM 写的流程没人看过就被每次 recall 当权威流程发出去。 |
