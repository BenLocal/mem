create table if not exists memory_embeddings (
  memory_id text primary key references memories (memory_id),
  tenant text not null,
  embedding_model text not null,
  embedding_dim integer not null,
  embedding blob not null,
  content_hash text not null,
  source_updated_at text not null,
  created_at text not null,
  updated_at text not null
);

create index if not exists idx_memory_embeddings_tenant on memory_embeddings (tenant);

create table if not exists embedding_jobs (
  job_id text primary key,
  tenant text not null,
  memory_id text not null references memories (memory_id),
  target_content_hash text not null,
  provider text not null,
  status text not null,
  attempt_count integer not null default 0,
  last_error text,
  available_at text not null,
  created_at text not null,
  updated_at text not null,
  constraint embedding_jobs_status_check check (
    status in ('pending', 'processing', 'completed', 'failed', 'stale')
  )
);

create index if not exists idx_embedding_jobs_poll on embedding_jobs (status, available_at);
create index if not exists idx_embedding_jobs_tenant_memory on embedding_jobs (tenant, memory_id);

-- Live-job dedupe for (tenant, memory_id, target_content_hash, provider) is enforced in application
-- code: DuckDB (bundled) does not support partial unique indexes here.
