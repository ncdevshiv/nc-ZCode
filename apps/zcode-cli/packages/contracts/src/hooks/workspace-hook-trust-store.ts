/**
* Trust store file schema has been sunk to `@zcode/shared/workspace-hook-trust-store-file`
* Implemented as a single authority (the services layer cannot rely on contracts under apps,
* Had to write partial verification by hand, causing the UI and runtime to split their conclusions on the same damaged file).
 *
* Here re-export keeps the existing import path of contracts (adapters/bootstrap/core, etc.)
* Unchanged; shared and contracts belong to zod4 / zod3 instances respectively, this re-export shall not be
* Zod3 schema composite reference inside contracts.
 */
export {
  WORKSPACE_HOOK_TRUST_STORE_SCHEMA_VERSION,
  workspaceHookTrustRecordSchema,
  workspaceHookTrustStoreFileSchema,
} from "@zcode/shared/workspace-hook-trust-store-file";
export type {
  WorkspaceHookTrustRecord,
  WorkspaceHookTrustStoreFile,
} from "@zcode/shared/workspace-hook-trust-store-file";

import { z } from "zod";

const nonEmptyStringSchema = z.string().trim().min(1);

export const workspaceHookPolicySchema = z.discriminatedUnion("mode", [
  z
    .object({
      mode: z.literal("deny"),
      reason: nonEmptyStringSchema,
      policyRevision: nonEmptyStringSchema,
    })
    .strict(),
  z
    .object({
      mode: z.literal("user_decides"),
      policyRevision: nonEmptyStringSchema,
    })
    .strict(),
  z
    .object({
      mode: z.literal("allow_trusted_only"),
      reason: nonEmptyStringSchema.optional(),
      policyRevision: nonEmptyStringSchema,
    })
    .strict(),
]);
export type WorkspaceHookPolicy = z.infer<typeof workspaceHookPolicySchema>;
