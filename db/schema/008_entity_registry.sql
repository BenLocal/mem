-- Entity registry: canonicalize alias strings to stable entity_id.
-- See docs/superpowers/specs/2026-05-02-entity-registry-design.md.
--
-- Two tables: entities (canonical record) + entity_aliases (lookup).
-- alias_text is the normalized form (lowercase + whitespace-collapsed) — see
-- normalize_alias() in src/pipeline/entity_normalize.rs. canonical_name is
-- the caller's verbatim original spelling.
--
-- Tenant-scoped (composite keys include tenant). Entities are NOT linked to
-- sessions: "Rust" written across multiple sessions resolves to the same
-- entity_id within a tenant.

create table if not exists entities (
    entity_id text primary key,
    tenant text not null,
    canonical_name text not null,
    kind text not null,
    created_at text not null,
    constraint entities_kind_check check (
        kind in ('topic', 'project', 'repo', 'module', 'workflow')
    )
);

create index if not exists idx_entities_tenant_kind
    on entities(tenant, kind);

create table if not exists entity_aliases (
    tenant text not null,
    alias_text text not null,
    entity_id text not null,
    created_at text not null,
    primary key (tenant, alias_text)
);

create index if not exists idx_entity_aliases_entity
    on entity_aliases(entity_id);

-- ALTER memories: caller-supplied verbatim topic strings (JSON-encoded
-- Vec<String>, NULL when omitted). Same storage shape as `evidence` field.
-- Note: DuckDB does not support `ADD COLUMN IF NOT EXISTS`. The schema
-- runner applies this file statement-by-statement and swallows the
-- "Column with name 'topics' already exists!" error on re-run, mirroring
-- the 004_sessions.sql ALTER handling.
alter table memories add column topics text;
