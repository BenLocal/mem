export const currentRawMemToolSurface = [
  "mem_health",
  "memory_search",
  "memory_ingest",
  "memory_get",
  "memory_feedback",
  "memory_apply_feedback",
  "memory_list_pending_review",
  "memory_review_accept",
  "memory_review_reject",
  "memory_review_edit_accept",
  "episode_ingest",
  "memory_graph_neighbors",
  "embeddings_list_jobs",
  "embeddings_rebuild",
  "embeddings_providers",
] as const;

export const rawToHighLevelMemoryToolMappings = [
  {
    rawTool: "memory_search",
    highLevelTool: "memory_bootstrap",
    method: "POST",
    path: "memories/search",
  },
  {
    rawTool: "memory_search",
    highLevelTool: "memory_search_contextual",
    method: "POST",
    path: "memories/search",
  },
  {
    rawTool: "memory_ingest",
    highLevelTool: "memory_commit_fact",
    method: "POST",
    path: "memories",
  },
  {
    rawTool: "episode_ingest",
    highLevelTool: "memory_propose_experience",
    method: "POST",
    path: "episodes",
  },
  {
    rawTool: "memory_ingest",
    highLevelTool: "memory_propose_preference",
    method: "POST",
    path: "memories",
  },
  {
    rawTool: "memory_feedback",
    highLevelTool: "memory_apply_feedback",
    method: "POST",
    path: "memories/feedback",
  },
] as const;

export const memoryBootstrapContract = {
  toolName: "memory_bootstrap",
  rawTool: "memory_search",
  method: "POST",
  path: "memories/search",
  defaultScope: "project",
  allowedScopes: ["project"] as const,
  requiredContextFields: [
    "tenant",
    "project",
    "caller_agent",
    "source_agent",
    "query",
  ] as const,
} as const;

export const memorySearchContextualContract = {
  toolName: "memory_search_contextual",
  rawTool: "memory_search",
  method: "POST",
  path: "memories/search",
  allowedScopes: ["project", "repo", "workspace", "global"] as const,
} as const;

export const memoryCommitFactContract = {
  toolName: "memory_commit_fact",
  rawTool: "memory_ingest",
  method: "POST",
  path: "memories",
  memoryKind: "fact",
} as const;

export const memoryProposeExperienceContract = {
  toolName: "memory_propose_experience",
  rawTool: "episode_ingest",
  method: "POST",
  path: "episodes",
  memoryKind: "experience",
} as const;

export const memoryProposePreferenceContract = {
  toolName: "memory_propose_preference",
  rawTool: "memory_ingest",
  method: "POST",
  path: "memories",
  memoryKind: "preference",
} as const;

export const memoryApplyFeedbackContract = {
  toolName: "memory_apply_feedback",
  rawTool: "memory_feedback",
  method: "POST",
  path: "memories/feedback",
  allowedKinds: ["useful", "outdated", "incorrect"] as const,
} as const;
