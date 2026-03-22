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
