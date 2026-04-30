-- Conversation archive: every block of every transcript message,
-- verbatim. Independent from memories table and its ranking/lifecycle.
-- See docs/superpowers/specs/2026-04-30-conversation-archive-design.md.
--
-- Note: DuckDB does not support inline REFERENCES via ALTER, but `CREATE
-- TABLE` does. session_id is intentionally declarative-only; the FK is
-- enforced at application level (mine.rs always passes a session_id that
-- the sessions table already knows about, since it comes from the
-- transcript). Same approach as memories.session_id (see 004_sessions.sql).

create table if not exists conversation_messages (
    message_block_id text primary key,
    session_id text,
    tenant text not null,
    caller_agent text not null,
    transcript_path text not null,
    line_number integer not null,
    block_index integer not null,
    message_uuid text,
    role text not null,
    block_type text not null,
    content text not null,
    tool_name text,
    tool_use_id text,
    embed_eligible boolean not null,
    created_at text not null,
    constraint conv_msg_role_check check (role in ('user','assistant','system')),
    constraint conv_msg_block_type_check check (block_type in ('text','tool_use','tool_result','thinking')),
    constraint conv_msg_uniq unique(transcript_path, line_number, block_index)
);

create index if not exists idx_conv_session_time
    on conversation_messages(session_id, created_at);

create index if not exists idx_conv_tenant_agent_time
    on conversation_messages(tenant, caller_agent, created_at);

create index if not exists idx_conv_tool_use_id
    on conversation_messages(tool_use_id);

-- Embedding queue: mirror of embedding_jobs but keyed to conversation_messages.
create table if not exists transcript_embedding_jobs (
    job_id text primary key,
    tenant text not null,
    message_block_id text not null,
    provider text not null,
    status text not null,
    attempt_count integer not null default 0,
    last_error text,
    available_at text not null,
    created_at text not null,
    updated_at text not null,
    constraint transcript_jobs_status_check check (
        status in ('pending', 'processing', 'completed', 'failed', 'stale')
    )
);

create index if not exists idx_transcript_jobs_poll
    on transcript_embedding_jobs(status, available_at);
create index if not exists idx_transcript_jobs_tenant_block
    on transcript_embedding_jobs(tenant, message_block_id);

-- Embedding storage: mirror of memory_embeddings but keyed to message_block_id.
create table if not exists conversation_message_embeddings (
    message_block_id text primary key,
    tenant text not null,
    embedding_model text not null,
    embedding_dim integer not null,
    embedding blob not null,
    content_hash text not null,
    source_updated_at text not null,
    created_at text not null,
    updated_at text not null
);

create index if not exists idx_conv_msg_emb_tenant on conversation_message_embeddings (tenant);
