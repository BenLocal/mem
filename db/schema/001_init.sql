create table if not exists memories (
  memory_id text primary key,
  tenant text not null,
  memory_type text not null,
  status text not null,
  scope text not null,
  visibility text not null,
  version integer not null,
  summary text not null,
  content text not null,
  evidence_json text not null,
  code_refs_json text not null,
  project text,
  repo text,
  module text,
  task_type text,
  tags_json text not null,
  confidence double not null,
  decay_score double not null,
  content_hash text not null,
  idempotency_key text,
  supersedes_memory_id text,
  source_agent text not null,
  created_at text not null,
  updated_at text not null,
  last_validated_at text
);

create index if not exists idx_memories_status on memories(status);
create index if not exists idx_memories_content_hash on memories(content_hash);
create index if not exists idx_memories_idempotency_key on memories(idempotency_key);
create index if not exists idx_memories_supersedes on memories(supersedes_memory_id);

create table if not exists episodes (
  episode_id text primary key,
  tenant text not null,
  goal text not null,
  steps_json text not null,
  outcome text not null,
  evidence_json text not null,
  scope text not null,
  visibility text not null,
  project text,
  repo text,
  module text,
  tags_json text not null,
  source_agent text not null,
  idempotency_key text,
  created_at text not null,
  updated_at text not null,
  workflow_candidate_json text
);

create index if not exists idx_episodes_idempotency_key on episodes(idempotency_key);
create index if not exists idx_episodes_repo_module on episodes(repo, module);

create table if not exists feedback_events (
  feedback_id text primary key,
  memory_id text not null,
  feedback_kind text not null,
  created_at text not null
);

create index if not exists idx_feedback_events_memory_id on feedback_events(memory_id);
