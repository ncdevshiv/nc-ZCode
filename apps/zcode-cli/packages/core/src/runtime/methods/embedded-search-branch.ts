import type { ModelToolContract } from "../deps.js";
import { registerBuiltInTools } from "../deps.js";
import type { AgentRuntimeInternal } from "../internal.js";
import { resolveEmbeddedSearchBranchCapability } from "../../embedded-search/capability.js";
import {
  resolveBuiltInToolAllowlist,
  resolveRuntimeDynamicWorkflowToolsIncluded,
} from "../helpers/tool-allowlist.js";
import { isToolNameDisallowed } from "../../tool/tool-visibility.js";

export function resolveRuntimeEmbeddedSearchEnabled(runtime: AgentRuntimeInternal): boolean {
  const builtInToolAllowlist = resolveBuiltInToolAllowlist(runtime.config);
  const decision = resolveEmbeddedSearchBranchCapability({
    bashAvailable:
      (builtInToolAllowlist === undefined || builtInToolAllowlist.includes("Bash")) &&
      !isToolNameDisallowed("Bash", runtime.config.toolDisallowlist),
  });
  return decision.useEmbeddedSearchBranch;
}

export function refreshBranchAwareBuiltInTools(runtime: AgentRuntimeInternal): void {
  const embeddedSearchEnabled = resolveRuntimeEmbeddedSearchEnabled(runtime);
  if (embeddedSearchEnabled) {
    runtime.registry.unregister("Glob");
    runtime.registry.unregister("Grep");
  }

  registerBuiltInTools(runtime.registry, {
    bashTimeoutPolicy: runtime.config.bashTimeoutPolicy,
    includeSkill: Boolean(runtime.skillPort),
    includeAgent: Boolean(runtime.subagentPort),
    embeddedSearchEnabled,
// This function is the **second**
// Register the entry, and deliberately only pass a simplified option set. For "Only true to register" gate (OffPeak / Cron / Workflow...)
// Omission is safe; but the polarity of the dynamic workflow grayscale gate is opposite - "open when absent", omitting is equivalent to shaving off the first assembly
// The ten tools will be added back as they are when the shell snapshot is initialized (registry.register will overwrite the items with the same name,
// silentDuplicateWarnings also swallows the warning, so the whole process is silent). Derivation therefore must be done with runtime-tools.ts
// Sharing the same helper, the judgment cannot be rewritten here.
    includeDynamicWorkflow: resolveRuntimeDynamicWorkflowToolsIncluded(runtime.config),
    agentProfiles: runtime.config.subagents?.profiles,
    allowedTools: resolveBuiltInToolAllowlist(runtime.config),
    disallowedTools: runtime.config.toolDisallowlist,
    silentDuplicateWarnings: true,
  });
  runtime.cachedTools = null;
}

export function filterEmbeddedSearchRuntimeVisibleTools(
  runtime: AgentRuntimeInternal,
  tools: ModelToolContract[],
): ModelToolContract[] {
  if (!resolveRuntimeEmbeddedSearchEnabled(runtime)) return tools;
  return tools.filter((tool) => tool.name !== "Glob" && tool.name !== "Grep");
}
