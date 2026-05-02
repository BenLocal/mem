# Entity Registry — Design

> Closes ROADMAP #8。让 graph 边和 memory 引用收敛到同一 canonical entity（'Rust' = 'Rust language' = 'rustlang' 都指向同一节点），消除当前 entity 名字直接当 string 用造成的碎片化。

## Summary

mem 现状：`pipeline::ingest::extract_graph_edges` 把 `MemoryRecord` 的 `project` / `repo` / `module` / `task_type` 字段直接拼成字符串 `to_node_id`（如 `"project:mem"`、`"repo:foo/bar"`）。两条 memory 写 `project="mem"` 和 `project="Mem"` 会产生两个不同节点；写 `topic="Rust"` 和 `topic="Rust language"` 也无法关联。Graph 查询碎片化，无法支撑"找所有讨论 Rust 的 memory"这类基础问题。

本设计新增 entity registry：
1. 两张表 `entities` + `entity_aliases`，alias 经 normalize 后作为 `(tenant, alias_text)` 复合 PK 直查
2. `MemoryRecord` 加 `topics: Vec<String>` 字段，caller 显式声明 entity 引用（非 NLP 自动抽取）
3. ingest 路径走"resolve-or-create"：alias 命中→返回 entity_id；不命中→自动建 entity + alias，返回新 id
4. `graph_edges.to_node_id` 从 `"project:mem"` 字符串变成 `"entity:<uuid>"` —— 新 ingest 立即生效，老数据通过 `mem repair --rebuild-graph` CLI 一次性重派
5. 4 条 HTTP 路由让 caller 显式管理 alias（建实体、查实体、加 alias、列表）

## Goals

- Tenant 内同义词收敛：`"Rust"` / `"rust"` / `"Rust "` / `"RUST"` 经 normalize 命中同一 entity_id
- Caller 显式 alias 加挂：`POST /entities/{id}/aliases {alias: "Rust language"}` 让此别名后续也解析到该 entity
- 与现有 graph_edges 兼容：schema 不变，仅写入端格式从 `"project:..."` 改为 `"entity:<uuid>"`，老数据通过 CLI 一次性迁移
- Verbatim 守则：caller 第一次写入的字符串 100% 进 `entities.canonical_name`；normalize 是 PK 索引内部细节，不破坏存储原文
- 与 sessions 正交：跨 session 的同义引用聚合到同一 entity（这是 #8 的目标，非缺陷）
- 零 NLP 依赖、零外部 model；纯 Rust 字符串规范化
- 不暴露 MCP（沿用 conversation-archive / transcript-recall 的 HTTP-only 约定）
- 不引入 entity bitemporal history、不引入 entity merge / delete / rename 操作（admin 留 v2）

## Non-Goals

- 自动从 `memory.content` NLP 抽取 entity（误判风险高，调参拖项目；caller 显式传 `topics` 即可）
- DELETE entity / merge two entities / rename canonical_name（admin 操作，v1 punt 到 v2）
- MCP transcript-style 工具暴露
- entity-level access control beyond tenant
- entity ranking / 语义搜索（list 仅 SQL `LIKE`）
- entity bitemporal history（entities 行 immutable except 未来 admin rename；entity_aliases 行 append-only）
- 全局（跨 tenant）entity registry —— tenant 是隔离边界
- 把 entity_id 作为 graph_edges.to_node_id 的 FK（DuckDB ALTER 限制 + 多 prefix 节点类型并存）
- 实时 entity 推荐 / autocomplete

## Decisions (resolved during brainstorming)

- **Q1 (entity 来源)**: B — 现有字段（project/repo/module/workflow）+ 新显式 `topics: Vec<String>` 字段。无 NLP 自动抽取。
- **Q2 (alias 存储形态)**: A — 两张表 `entities` + `entity_aliases`；entity_id (UUIDv7) 稳定，canonical 重命名不破引用；alias_text 是复合 PK 的一部分实现 O(1) 哈希查找。
- **Q3 (normalization 规则)**: C — 大小写折叠 + trim + 内部空白 collapse。**保留标点**（C++、C#、.NET 这类靠标点区分身份）。**不**做 Unicode NFKC（YAGNI；未来真发现全角/智能引号问题再加）。
- **Q4 (新 entity 入册规则)**: B — 未知 alias 自动晋升为新 entity（caller-friendly UX）；admin merge / rename 留 v2；`POST /entities/{id}/aliases` 让 caller 后续显式合并同义。
- **Q5 (graph_edges 迁移)**: C — schema 008 只建表不动数据；新 ingest 立即走 registry；老数据通过新 CLI `mem repair --rebuild-graph` 一次性从 memories 重派（pure-function 派生，零 bespoke 迁移代码）。
- **Q6 (sessions / verbatim layering)**: 整包 — tenant 级作用域；与 sessions 正交（无 session_id 外键）；canonical_name verbatim、alias_text 是 normalize PK；entity 行 v1 immutable，alias 行 append-only；不复用 graph_edges 的 bitemporal。

## Architecture

```
┌───────────────────────────────────────────────────────────────────────────┐
│                           POST /memories（ingest）                          │
│   IngestMemoryRequest 加 topics: Option<Vec<String>>（caller 原文）         │
└──────────────────────────────────┬────────────────────────────────────────┘
                                   ▼
              pipeline::ingest::extract_graph_edge_drafts（纯函数）
                       │ 对每个 project/repo/module/workflow + 每个 topic：
                       │ 产出 GraphEdgeDraft { from, to_kind: EntityRef(kind, alias), relation }
                       ▼
              service::memory_service::resolve_drafts_to_edges（async, has DB）
                       │ 每个 draft 调 EntityRegistry::resolve_or_create:
                       ▼
              EntityRegistry::resolve_or_create(tenant, alias, kind, now) -> entity_id
                       │
              ┌────────┴───────────────┐
              ▼                        ▼
      alias hit (PK 命中)      alias miss (auto-promote)
      return entity_id          单 mutex hold 内：
                                INSERT entities + INSERT entity_aliases
                                return new entity_id
                       │
                       ▼
              graph_edges.to_node_id = "entity:<entity_id>"


┌──────── 新表 schema 008 ──────────────────────────────────────────────────┐
│  entities                              entity_aliases                     │
│  ┌──────────────────────┐              ┌──────────────────────────┐      │
│  │ entity_id (PK, UUID7)│◄─────────────┤ tenant      ┐ composite  │      │
│  │ tenant               │              │ alias_text  ┘ PK         │      │
│  │ canonical_name       │              │ entity_id (FK)           │      │
│  │   (verbatim caller)  │              │ created_at               │      │
│  │ kind                 │              └──────────────────────────┘      │
│  │ created_at           │              alias_text 是 normalize 后形式    │
│  └──────────────────────┘              (lowercase + whitespace collapsed) │
│                                                                            │
│  + ALTER TABLE memories ADD COLUMN topics text;  -- JSON array            │
└────────────────────────────────────────────────────────────────────────────┘


┌──────── HTTP routes（admin / explicit alias mgmt） ───────────────────────┐
│   POST   /entities                  → 显式建 entity（含 aliases 数组）     │
│   GET    /entities/{id}             → 详情 + aliases 列表                 │
│   POST   /entities/{id}/aliases     → 加 alias，幂等；冲突返 409          │
│   GET    /entities?tenant=&kind=&q= → 列表 + LIKE 过滤（无 ranking）      │
│   不暴露 MCP                                                              │
└────────────────────────────────────────────────────────────────────────────┘


┌──────── Migration ────────────────────────────────────────────────────────┐
│   schema 008：建 entities + entity_aliases；ALTER memories ADD topics     │
│   新 ingest 立即走 registry（"entity:<uuid>" 格式）                        │
│   `mem repair --rebuild-graph`（新 CLI 子命令）：                          │
│     1. DELETE FROM graph_edges WHERE from_node_id LIKE 'memory:%'         │
│     2. SELECT * FROM memories WHERE tenant = ?                            │
│     3. for each memory: extract_graph_edge_drafts + resolve + INSERT      │
│     4. 老 "project:..." 字符串全部转成 "entity:<uuid>"                     │
│   幂等；先 alias 命中复用现有 entity_id                                    │
└────────────────────────────────────────────────────────────────────────────┘


┌──────── Layering ─────────────────────────────────────────────────────────┐
│   📦 storage：entities + entity_aliases；canonical_name verbatim          │
│   🔍 indexing：extract_graph_edges 走 registry，to_node_id 是 entity_id    │
│   ⚙️  infra：1 schema 文件、1 CLI 子命令、0 新依赖                          │
│                                                                            │
│   verbatim 守则：caller 原文进 canonical_name；normalize 仅在 PK 索引层   │
│   sessions 正交：entities 跨 session 收敛（这是 #8 目标）                  │
└────────────────────────────────────────────────────────────────────────────┘
```

## Schema

新文件 `db/schema/008_entity_registry.sql`（append-only；不修改 001–007）：

```sql
-- Entity registry: canonicalize alias strings to stable entity_id.
-- See docs/superpowers/specs/2026-05-02-entity-registry-design.md.
--
-- Two tables: entities (canonical record) + entity_aliases (lookup).
-- alias_text is the normalized form (lowercase + whitespace-collapsed); see
-- normalize_alias() in src/pipeline/entity_normalize.rs. canonical_name is
-- the caller's verbatim original spelling.
--
-- Tenant-scoped (composite keys include tenant). Entities are NOT linked to
-- sessions: "Rust" written across multiple sessions resolves to the same
-- entity_id within a tenant.

create table if not exists entities (
    entity_id text primary key,                    -- UUIDv7, server-minted
    tenant text not null,
    canonical_name text not null,                  -- caller verbatim (first-writer-wins)
    kind text not null,                             -- 'topic' | 'project' | 'repo' | 'module' | 'workflow'
    created_at text not null,
    constraint entities_kind_check check (
        kind in ('topic', 'project', 'repo', 'module', 'workflow')
    )
);

create index if not exists idx_entities_tenant_kind
    on entities(tenant, kind);

create table if not exists entity_aliases (
    tenant text not null,
    alias_text text not null,                       -- normalized form
    entity_id text not null,                        -- FK to entities.entity_id (app-enforced)
    created_at text not null,
    primary key (tenant, alias_text)
);

create index if not exists idx_entity_aliases_entity
    on entity_aliases(entity_id);

-- ALTER memories: caller-supplied verbatim topic strings (JSON-encoded
-- Vec<String>; NULL when omitted). Same storage shape as `evidence` field.
alter table memories add column topics text;

-- DuckDB caveats:
-- * No inline FK on entity_aliases.entity_id (DuckDB enforcement is partial);
--   resolve_or_create always inserts entities row first, then alias row,
--   under a single Arc<Mutex<Connection>> hold.
-- * ALTER TABLE ADD COLUMN re-run requires schema_runner to swallow the
--   "Column with name 'topics' already exists!" error (same handling as
--   004_sessions.sql session_id; extend the existing branch in schema.rs).
-- * `tenant` is text (not enum) — matches memories/conversation_messages.
-- * `created_at` uses 20-digit zero-padded ms-since-epoch (current_timestamp).
```

### Storage 注解

- **`entities` PK = `entity_id`**：稳定，rename canonical 不破引用。
- **`entity_aliases` PK = `(tenant, alias_text)`**：tenant 内 alias 唯一；O(1) 哈希直查；同一 entity 多 alias 通过 `idx_entity_aliases_entity` 反查。
- **`kind` CHECK 约束**：5 个固定值，硬保护 typo 不写脏 registry。未来加新 kind 走单独 schema 文件改 CHECK。
- **`memories.topics`**：JSON-encoded `Vec<String>`，NULL 表示未传，`'[]'` 表示显式空。caller 原文（含原始大小写/空白）直接 encode；DAO 层读取时 `unwrap_or_default()` 容错。

## Domain Types

### `src/domain/entity.rs`（新文件）

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entity {
    pub entity_id: String,
    pub tenant: String,
    pub canonical_name: String,
    pub kind: EntityKind,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum EntityKind {
    Topic,
    Project,
    Repo,
    Module,
    Workflow,
}

impl EntityKind {
    pub fn as_db_str(self) -> &'static str { /* match self { Topic => "topic", ... } */ }
    pub fn from_db_str(s: &str) -> Option<Self> { /* reverse */ }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntityWithAliases {
    pub entity: Entity,
    pub aliases: Vec<String>,  // normalized forms; ordered by created_at asc
}

/// Result of `EntityRegistry::add_alias`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddAliasOutcome {
    Inserted,
    AlreadyOnSameEntity,
    ConflictWithDifferentEntity(String),  // existing owner's entity_id
}
```

### `src/domain/memory.rs`（修改）

`MemoryRecord` 加：

```rust
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub topics: Vec<String>,
```

`IngestMemoryRequest`（在 `src/http/memory.rs`）同样加：

```rust
#[serde(default)]
pub topics: Vec<String>,
```

## Pipeline 改动

### `src/pipeline/entity_normalize.rs`（新文件）

```rust
//! Pure normalize_alias function shared by EntityRegistry and pipeline.

/// Lowercase + trim + collapse internal whitespace.
/// Punctuation and Unicode are preserved verbatim (no NFKC).
pub fn normalize_alias(s: &str) -> String {
    s.split_whitespace()           // strips runs of whitespace, also trims
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn collapses_case_and_whitespace() {
        assert_eq!(normalize_alias("Rust"), "rust");
        assert_eq!(normalize_alias("  Rust  "), "rust");
        assert_eq!(normalize_alias("Rust  language"), "rust language");
        assert_eq!(normalize_alias("\t Rust\nLanguage \t"), "rust language");
    }
    #[test]
    fn preserves_punctuation_and_unicode() {
        assert_eq!(normalize_alias("C++"), "c++");
        assert_eq!(normalize_alias("C#"), "c#");
        assert_eq!(normalize_alias(".NET"), ".net");
        assert_eq!(normalize_alias("F#"), "f#");
        assert_eq!(normalize_alias("中文"), "中文");
        assert_eq!(normalize_alias("Naïve"), "naïve");
    }
    #[test]
    fn empty_and_whitespace_only() {
        assert_eq!(normalize_alias(""), "");
        assert_eq!(normalize_alias("   "), "");
    }
}
```

### `src/pipeline/ingest.rs`（修改）

拆出 pure draft step，保留兼容 wrapper：

```rust
#[derive(Debug, Clone)]
pub enum ToNodeKind {
    EntityRef { kind: EntityKind, alias: String },
    LiteralMemory(String),  // memory_id reference
    // future: other prefix kinds
}

#[derive(Debug, Clone)]
pub struct GraphEdgeDraft {
    pub from_node_id: String,         // already in "memory:<id>" form
    pub to_kind: ToNodeKind,
    pub relation: String,
}

/// Pure: produces drafts without resolving entities. Used by both:
///   - service::memory_service::ingest (resolves drafts via EntityRegistry)
///   - cli::repair::rebuild_graph (same resolution path on historical memories)
pub fn extract_graph_edge_drafts(memory: &MemoryRecord) -> Vec<GraphEdgeDraft> {
    // For each non-empty field:
    //   memory.project    → kind=Project,  relation="applies_to"
    //   memory.repo       → kind=Repo,     relation="observed_in"
    //   memory.module     → kind=Module,   relation="relevant_to"
    //   memory.task_type  → kind=Workflow, relation="uses_workflow"
    //   memory.topics[*]  → kind=Topic,    relation="discusses"   (NEW)
    // All produce GraphEdgeDraft { from_node_id: memory_node_id(memory.memory_id),
    //                              to_kind: EntityRef { kind, alias: <field value> },
    //                              relation }
}

/// Backwards-compat: existing tests / callers that don't have a registry.
/// Produces edges with the OLD string format ("project:mem") for the entity-ref
/// edges. NOT recommended for new code; the new path goes through
/// resolve_drafts_to_edges with a registry.
#[deprecated(note = "Use extract_graph_edge_drafts + resolve via EntityRegistry")]
pub fn extract_graph_edges(memory: &MemoryRecord) -> Vec<GraphEdge> {
    extract_graph_edge_drafts(memory)
        .into_iter()
        .map(|draft| /* assemble legacy string to_node_id */)
        .collect()
}
```

### `src/service/memory_service.rs`（修改）

ingest 路径加 resolution 步骤：

```rust
async fn resolve_drafts_to_edges(
    drafts: Vec<GraphEdgeDraft>,
    registry: &impl EntityRegistry,
    tenant: &str,
    now: &str,
) -> Result<Vec<GraphEdge>, StorageError> {
    let mut edges = Vec::with_capacity(drafts.len());
    for draft in drafts {
        let to_node_id = match draft.to_kind {
            ToNodeKind::EntityRef { kind, alias } => {
                let id = registry.resolve_or_create(tenant, &alias, kind, now).await?;
                format!("entity:{id}")
            }
            ToNodeKind::LiteralMemory(memory_id) => format!("memory:{memory_id}"),
        };
        edges.push(GraphEdge {
            from_node_id: draft.from_node_id,
            to_node_id,
            relation: draft.relation,
            valid_from: now.to_string(),
            valid_to: None,
        });
    }
    Ok(edges)
}
```

`MemoryService::ingest` 的 graph 部分调用 `extract_graph_edge_drafts(memory)` 然后 `resolve_drafts_to_edges(...)`。

## Storage Layer

### `src/storage/duckdb.rs`（修改）

```rust
pub trait EntityRegistry {
    async fn resolve_or_create(
        &self,
        tenant: &str,
        alias: &str,        // caller verbatim
        kind: EntityKind,
        now: &str,
    ) -> Result<String, StorageError>;

    async fn get_entity(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError>;

    async fn add_alias(
        &self,
        tenant: &str,
        entity_id: &str,
        alias: &str,
        now: &str,
    ) -> Result<AddAliasOutcome, StorageError>;

    async fn list_entities(
        &self,
        tenant: &str,
        kind_filter: Option<EntityKind>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>, StorageError>;
}
```

### `src/storage/entity_repo.rs`（新文件）

`impl EntityRegistry for DuckDbRepository` — 与 `transcript_repo.rs` 同模式。关键 invariant：`resolve_or_create` 在单次 `Arc<Mutex<Connection>>` 锁占用期间完成 lookup + 两条 INSERT，禁止释放锁。

`add_alias` 的冲突处理：

```rust
// 先 lookup 当前 alias 的 owner
let existing: Option<String> = SELECT entity_id FROM entity_aliases
    WHERE tenant = ?1 AND alias_text = ?2;
match existing {
    None => INSERT row → AddAliasOutcome::Inserted
    Some(owner) if owner == entity_id => AddAliasOutcome::AlreadyOnSameEntity
    Some(owner) => AddAliasOutcome::ConflictWithDifferentEntity(owner)
}
// 全过程在单 mutex hold 内
```

## HTTP Layer

### `src/http/entities.rs`（新文件）

4 条路由：

| Verb | Path | Body | Response |
|---|---|---|---|
| `POST` | `/entities` | `{ tenant, canonical_name, kind, aliases?: Vec<String> }` | `201` + `EntityWithAliases`；冲突 alias 返 `409` 列出冲突 owner |
| `GET` | `/entities/{id}?tenant=...` | — | `200` + `EntityWithAliases`，`404` 不存在 |
| `POST` | `/entities/{id}/aliases` | `{ tenant, alias }` | `200` + `{ outcome: "inserted" \| "already_on_same_entity" }`；冲突 `409` + `{ existing_entity_id }` |
| `GET` | `/entities?tenant=...&kind=...&q=...&limit=` | — | `200` + `{ entities: Vec<Entity> }`，按 created_at desc，limit ≤ 100 |

错误映射（沿用 transcript-recall final-review 教训）：使用 `AppError`，不直接 `(StatusCode, String)`：
- `StorageError::NotFound` → 500（避免 id 泄漏）
- 业务级 404（entity 不存在）→ HTTP handler 显式返 `(StatusCode::NOT_FOUND, ...)`
- 409 冲突 → HTTP handler 显式返

## CLI

### `src/cli/repair.rs`（修改）

加 `--rebuild-graph` flag 与 `--check` / `--rebuild` 同级：

```rust
#[derive(Args)]
struct RepairArgs {
    #[arg(long)] check: bool,
    #[arg(long)] rebuild: bool,           // 既有：重建 vector_index sidecar
    #[arg(long)] rebuild_graph: bool,     // 新增
    // ...
}
```

实现：

```rust
async fn run_rebuild_graph(repo: &DuckDbRepository, registry: &impl EntityRegistry) -> Result<(), AppError> {
    let tenants: Vec<String> = repo.list_distinct_tenants().await?;
    for tenant in tenants {
        let memories: Vec<MemoryRecord> = repo.list_memories_for_tenant(&tenant).await?;
        repo.delete_graph_edges_originating_from_memories(&tenant).await?;  // DELETE WHERE from_node_id LIKE 'memory:%' AND ... 
        for memory in memories {
            let drafts = extract_graph_edge_drafts(&memory);
            let edges = resolve_drafts_to_edges(drafts, registry, &tenant, &now()).await?;
            repo.bulk_insert_graph_edges(&edges).await?;
        }
    }
    Ok(())
}
```

输出格式与现有 `--check` / `--rebuild` 风格一致：text 行 + `--json` flag。

## Testing Strategy

### Unit (in source files)

- `pipeline::entity_normalize::tests` — 7 cases（case/空白/标点/Unicode/empty）
- `domain::entity::tests` — `EntityKind` round-trip
- `pipeline::ingest::tests` — `extract_graph_edge_drafts` 5 字段组合（tests 已存在的 `extract_graph_edges` test 通过 deprecated wrapper 保持通过）

### Integration（新文件 `tests/entity_registry.rs`，~18 个测试）

**Storage 层（10）**：

1. `schema_creates_entities_and_aliases_tables` — bootstrap 后两表存在；`memories.topics` 列存在
2. `resolve_or_create_inserts_entity_and_alias_on_first_call`
3. `resolve_or_create_is_idempotent_on_alias_hit`（含大小写/空白变体均命中）
4. `resolve_or_create_creates_separate_entities_for_distinct_aliases`（caller 没声明同义则各自独立）
5. `add_alias_links_to_existing_entity` — 后续 resolve 命中合并后的 entity
6. `add_alias_returns_already_on_same_entity_when_idempotent`
7. `add_alias_returns_conflict_when_alias_belongs_to_different_entity`
8. `tenant_isolation_distinct_registries`
9. `kind_check_constraint_rejects_invalid`
10. `list_entities_filters_by_kind_and_query`

**HTTP 层（5）**：

11. `post_entities_creates_with_aliases`
12. `get_entities_returns_canonical_and_aliases`
13. `post_entity_aliases_idempotent_and_409_on_conflict`
14. `ingest_with_topics_creates_entities_and_graph_edges`
15. `ingest_existing_topic_resolves_no_duplicate_entity`

### Migration / `mem repair --rebuild-graph`（extend `tests/repair_cli.rs`，3 个）

16. `rebuild_graph_converts_legacy_to_entity_refs` — seed 老格式 graph_edges + 新格式 memories → 跑 → 全部转新
17. `rebuild_graph_is_idempotent`
18. `rebuild_graph_handles_empty_database`

### Memories 回归

- 现有 `tests/graph_temporal.rs`、其他用 `extract_graph_edges` 的测试通过 deprecated wrapper 保持 BC
- `cargo test --test search_api / bm25_search / hybrid_search / transcript_recall / conversation_archive` 全过

## Risks

1. **DuckDB 复合 PK 上的 `ON CONFLICT (tenant, alias_text) DO NOTHING`** — 单 PK 已验证（conversation-archive `INSERT OR IGNORE`），复合 PK 需要 plan Task 1 的 `#[ignore]` probe 测试确认；不支持时退到 SELECT-then-INSERT。
2. **`mem repair --rebuild-graph` 重建后丢失 `valid_from/valid_to` 历史** — v1 选简单方案：重建即"用当前 registry 重新产生 active edges"，已 closed 的历史边丢失。commit message 写明这是"first-run upgrade migration loses temporal history of graph_edges"。数据集小、目前无 valid_to 历史查询用户，可接受。未来需要再做精细迁移。
3. **`ALTER TABLE memories ADD COLUMN topics` 重跑非幂等** — DuckDB 不支持 `ADD COLUMN IF NOT EXISTS`；schema_runner 现有 sessions 那段"按语句拆 + 容错 already exists"分支需要扩展覆盖 008。实施者要把 008 加入此分支。
4. **`extract_graph_edges` 兼容 wrapper 的语义漂移** — 老调用点（如 `tests/graph_temporal.rs`）仍用字符串 to_node_id；新调用点用 entity_id。两路径长期共存，未来若有新业务想"找所有引用某 entity 的 graph 边"必须用新路径。`#[deprecated]` 注解 + 文档警告防止误用。
5. **JSON `topics` 列序列化错误** — caller 传非法（HTTP 层 serde 拦下，400）；存储层读取 NULL/旧空 → DAO `unwrap_or_default()` 兜底为 `Vec::new()`。
6. **arphan entities** — Q4 接受 typo 自动晋升，导致 `"rsut"` 这类 typo 留作孤儿 entity（无 graph_edges 引用）。v1 不清理；v2 admin merge / delete 时一并处理。个人项目预期 entity 数 < 1000，此噪音可容忍。

## Concerns to Confirm Before Implementing

1. **DuckDB ON CONFLICT 复合 PK 支持** — Task 1 用 `#[ignore]` probe 测试探测；不支持时退到 SELECT-then-INSERT。
2. **schema_runner 的 ALTER 重跑兜底分支** — 实施者读 `src/storage/schema.rs` 现有 004_sessions 那段处理，把 008 的 ALTER 也纳入。
3. **`extract_graph_edges` 现有调用点** — implementer 第一步 `grep -rn 'extract_graph_edges' src/ tests/`，确认调用点；deprecated wrapper 必须保留所有现有调用点的行为。

## Out of Scope (this PR)

- DELETE entity / merge entities / rename canonical_name（admin v2）
- MCP entity 工具暴露
- entity 语义搜索 / ranking
- 自动 NLP 抽取
- bitemporal entity history
- 全局（跨 tenant）registry
- entity_id 在 graph_edges 上加 FK
- 生产历史数据的 valid_to 精细保留迁移
- entity-level access control beyond tenant

## Verification Checklist (pre-merge)

- `cargo test -q --no-fail-fast` — 0 failures, ≤1 ignored（仅 FTS predicate probe）
- `cargo fmt --check` — 干净
- `cargo clippy --all-targets -- -D warnings` — 干净
- `cargo build --release` — 干净
- 手动冒烟：
  1. 全新 DB → POST `/entities` 显式建 `{canonical_name:"Rust", kind:"topic", aliases:["Rust language","rustlang"]}` 
  2. POST `/memories {topics:["rustlang"]}` → 验证 graph_edges 指向同一 entity_id
  3. POST `/entities/{id}/aliases {alias:"Rust 语言"}` → 200 OK
  4. POST `/memories {topics:["Rust 语言"]}` → 命中现有 entity_id
  5. 老格式遗留 graph_edges 行 → `cargo run -- repair --rebuild-graph` → 全部转 `entity:<uuid>` 格式
- 新加 ~18 集成测试全过；现有 `tests/graph_temporal.rs` 等通过 deprecated wrapper 保持 BC

## References

- `docs/superpowers/specs/2026-04-30-conversation-archive-design.md` — 上一波 spec（HTTP-only 约定模板）
- `docs/superpowers/specs/2026-05-01-transcript-recall-design.md` — 上一波 spec（zero-shared-state 与 pipeline/ranking.rs 抽取模式）
- `db/schema/003_graph.sql` — graph_edges schema（不动）
- `db/schema/004_sessions.sql` — ALTER memories 重跑兜底先例
- `src/pipeline/ingest.rs::extract_graph_edges` — 重构起点
- `src/storage/transcript_repo.rs` — `impl Trait for DuckDbRepository` 模式参考
- `src/cli/repair.rs` — `--rebuild-graph` flag 添加位置
- ROADMAP.MD — 本 spec 关闭 #8
