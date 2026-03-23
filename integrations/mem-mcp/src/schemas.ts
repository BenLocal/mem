import { z } from "zod";

export const memoryTypeZ = z.enum([
  "implementation",
  "experience",
  "preference",
  "episode",
  "workflow",
]);

export const scopeZ = z.enum(["global", "project", "repo", "workspace"]);

export const visibilityZ = z.enum(["private", "shared", "system"]);

export const writeModeZ = z.enum(["auto", "propose"]);

export const feedbackKindZ = z.enum([
  "useful",
  "outdated",
  "incorrect",
  "applies_here",
  "does_not_apply_here",
]);

export const highLevelFeedbackKindZ = z.enum([
  "useful",
  "outdated",
  "incorrect",
]);

export const memoryKindZ = z.enum(["fact", "experience", "preference"]);
const nonEmptyTrimmedStringZ = z.string().trim().min(1);
const nonEmptyTrimmedStringArrayZ = z.array(nonEmptyTrimmedStringZ).min(1);

export const highLevelMemoryContextZ = z.object({
  tenant: z.string().min(1),
  project: z.string().min(1),
  repo: z.string().min(1).optional(),
  module: z.string().min(1).optional(),
  caller_agent: z.string().min(1),
  source_agent: z.string().min(1),
});

export const highLevelBootstrapContextZ = highLevelMemoryContextZ.omit({
  source_agent: true,
});

export const memoryBootstrapInputZ = highLevelBootstrapContextZ.extend({
  query: z.string().min(1),
  scope: z.literal("project").default("project"),
  token_budget: z.number().int().positive().optional().default(120),
});

export const memorySearchContextualInputZ = highLevelMemoryContextZ.extend({
  query: z.string().min(1),
  intent: z.enum(["implementation", "debugging", "review"]),
  include_repo: z.boolean().optional().default(false),
  include_personal: z.boolean().optional().default(false),
  token_budget: z.number().int().positive().optional().default(400),
}).omit({
  source_agent: true,
}).superRefine((value, ctx) => {
  if (value.include_repo && value.repo === undefined) {
    ctx.addIssue({
      code: z.ZodIssueCode.custom,
      path: ["repo"],
      message: "repo is required when include_repo is true",
    });
  }
});

export const memoryCommitFactToolInputZ = highLevelMemoryContextZ.extend({
  project: nonEmptyTrimmedStringZ,
  repo: nonEmptyTrimmedStringZ.optional(),
  module: nonEmptyTrimmedStringZ.optional(),
  caller_agent: nonEmptyTrimmedStringZ,
  source_agent: nonEmptyTrimmedStringZ,
  summary: nonEmptyTrimmedStringZ,
  content: nonEmptyTrimmedStringZ,
  evidence: nonEmptyTrimmedStringArrayZ,
  tags: z.array(nonEmptyTrimmedStringZ).optional().default([]),
  idempotency_key: nonEmptyTrimmedStringZ.optional(),
});

export const memoryProposeExperienceInputZ = highLevelMemoryContextZ.extend({
  summary: nonEmptyTrimmedStringZ,
  content: nonEmptyTrimmedStringZ,
  evidence: z.array(nonEmptyTrimmedStringZ).optional().default([]),
});

export const memoryProposePreferenceInputZ = highLevelMemoryContextZ.extend({
  summary: nonEmptyTrimmedStringZ,
  content: nonEmptyTrimmedStringZ,
  evidence: z.array(nonEmptyTrimmedStringZ).optional().default([]),
});

export const memoryApplyFeedbackInputZ = z.object({
  tenant: z.string().min(1),
  project: z.string().min(1),
  caller_agent: z.string().min(1),
  memory_id: z.string().min(1),
  kind: highLevelFeedbackKindZ,
  note: z.string().optional(),
});
