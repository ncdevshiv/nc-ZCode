// Composer Skill read-only catalog.
// Separate from the Skills Settings management interface: the sessionId here determines the authority and does not provide start, stop/delete capabilities.
import {
  zcodeSkillsReferenceCatalogParamsSchema,
  type ZCodeSkillReferenceCatalogEntry,
  type ZCodeSkillsReferenceCatalogResult,
} from "@zcode/shared";
import type { SkillLoadOutcome, SkillMetadata } from "@zcode/contracts";
import { listZCodeSkills } from "../skills.js";
import {
  parseParams,
  requireSession,
  type ZCodeProtocolAgentServerContext,
} from "./server-types.js";

export async function getSkillReferenceCatalog(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
): Promise<ZCodeSkillsReferenceCatalogResult> {
  const params = parseParams(zcodeSkillsReferenceCatalogParamsSchema, rawParams);
  if (params.sessionId) {
    const record = requireSession(context, params.sessionId);
    const outcome = await record.app.getSkillCatalog();
    return toResult("session", outcome);
  }

// The old UI cache is only scanned when the workspace is mounted for the first time, and the user manually adds it from the file system.
// After Skill, the new conversation is still stuck at the old snapshot. The draft request reuses the CLI formal discovery configuration on the Agent side,
// Each time the reference panel is newly opened, the current workspace catalog is read.
  const outcome = await listZCodeSkills({
    env: context.deps.env,
    logger: context.logger,
    workingDirectory: params.workspace.workspacePath,
  });
  return toResult("workspace", outcome);
}

function toResult(
  authority: ZCodeSkillsReferenceCatalogResult["authority"],
  outcome: SkillLoadOutcome,
): ZCodeSkillsReferenceCatalogResult {
  return {
    authority,
    skills: outcome.skills.map(toReferenceCatalogEntry),
  };
}

function toReferenceCatalogEntry(skill: SkillMetadata): ZCodeSkillReferenceCatalogEntry {
  const scope =
    skill.source === "plugin" ? "plugin" : skill.scope === "project" ? "workspace" : "user";
  return {
// `glm:` is an existing UI provider filter contract; the path enables stable row identity for different sources with the same name.
    id: `glm:${scope}:${skill.path}`,
    name: skill.name,
    description: skill.description,
    path: skill.path,
    scope,
    enabled: true,
    ...(skill.pluginName ? { pluginName: skill.pluginName } : {}),
  };
}
