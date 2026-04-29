-- Sessions: time-window containers for memories captured by a single
-- caller_agent. Auto-bucketed on ingest by pipeline::session::resolve_session.
-- Closes ROADMAP #10. See docs/superpowers/specs/2026-04-29-sessions-design.md.
--
-- caller_agent is populated from IngestMemoryRequest.source_agent (same value;
-- the schema uses caller_agent as the canonical name for the actor field).

create table if not exists sessions (
    session_id text primary key,
    tenant text not null,
    caller_agent text not null,
    started_at text not null,
    last_seen_at text not null,
    ended_at text,
    goal text,
    memory_count integer not null default 0
);

create index if not exists idx_sessions_agent_active
    on sessions(tenant, caller_agent, ended_at);

-- Note: DuckDB does not support `ALTER TABLE ADD COLUMN ... REFERENCES` (parser
-- limitation). The session_id column is added without an inline FK constraint.
-- Application-level integrity is enforced by resolve_session(), which always
-- opens or continues a sessions row before writing the memory. See
-- docs/superpowers/specs/2026-04-29-sessions-design.md §DuckDB caveats.
alter table memories add column session_id text;

create index if not exists idx_memories_session on memories(session_id);
