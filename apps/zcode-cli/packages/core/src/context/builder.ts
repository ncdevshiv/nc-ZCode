// ============================================================
// Context Builder - System prompt assembly
// ============================================================

import type { ModelInputMessage } from "@zcode/contracts";
import type {
  ContextMetaUserAttachment,
  ContextSection,
  ContextBuildResult,
  ContextBuilderConfig,
  EnvInfo,
} from "./types.js";
import type { ToolRegistry } from "../tool/registry.js";
import { estimateTokens } from "./utils.js";
import { buildCliPrefixSection } from "./sections/cli-prefix.js";
import { buildIdentitySection } from "./sections/identity.js";
import { buildWorkflowActorIdentitySection } from "./sections/workflow-actor.js";
import { buildEnvInfoSection, buildGitSystemContextSection } from "./sections/env-info.js";
import { buildSkillsSection } from "./sections/skills.js";
import { buildRequestUserContextSection } from "./sections/request-user-context.js";
import { buildCurrentDateSection } from "./sections/current-date.js";
import { buildMemorySection } from "./sections/memory.js";
import { buildDesktopContextSection } from "./sections/desktop.js";
import {
  buildContextManagementSection,
  buildDynamicBehaviorSection,
  buildOutputStyleSection,
  buildSessionGuidanceSection,
} from "./dynamic-sections.js";

// -----------------------------------------------
// Context Builder
// -----------------------------------------------

const EPHEMERAL_CACHE_CONTROL = { type: "ephemeral" as const };
/** The registered name of the Skill tool (same literal as the metadata.name of tool/handlers/skill.ts; contracts has no constants). */
const SKILL_TOOL_NAME = "Skill";

export class ContextBuilder {
  private config: ContextBuilderConfig;
  private customSections: ContextSection[] = [];

  constructor(config: ContextBuilderConfig) {
    this.config = config;
  }

  /**
* Keep compatible entries. Tool descriptions are carried by the tools field of the model request and are no longer mirrored into the system prompt.
   */
  setToolRegistry(_registry: ToolRegistry): this {
    return this;
  }

  setEnvInfo(envInfo: EnvInfo): this {
    this.config = {
      ...this.config,
      envInfo,
    };
    return this;
  }

  /**
* Add custom section (for subsequent expansion)
   */
  addSection(
    section: Omit<ContextSection, "chars" | "tokens" | "injectionTarget" | "cacheHint"> &
      Partial<Pick<ContextSection, "injectionTarget" | "cacheHint">>,
  ): this {
    this.customSections.push({
      ...section,
      injectionTarget: section.injectionTarget ?? "system",
      cacheHint: section.cacheHint ?? "dynamic",
      chars: section.content.length,
      tokens: estimateTokens(section.content),
    });
    return this;
  }

  /**
* Build context and return structured results
   */
  build(): ContextBuildResult {
    const sections: ContextSection[] = [];
    const activeOutputStyle = this.config.outputStyle?.prompt.trim()
      ? this.config.outputStyle
      : undefined;
    const customSystemPrompt = this.config.customSystemPrompt?.trim();
    const hasCustomSystemPrompt = Boolean(customSystemPrompt);
// Workflow subagent identity: third path. and
// customSystemPrompt is mutually exclusive - the presence of both can only be a wiring error (persona should come in through workflowActor,
// Shouldn't block systemPrompt anymore), fail loudly instead of choosing one silently.
    const workflowActor = this.config.workflowActor;
    if (workflowActor !== undefined && hasCustomSystemPrompt) {
      throw new Error(
        "ContextBuilder: workflowActor and customSystemPrompt are mutually exclusive",
      );
    }
    const isWorkflowActor = workflowActor !== undefined;

    // 1. CLI / product prefix. Keep this as the short leading identity block.
// "You are ZCode, an interactive coding agent" versus one
// A subagent that only speaks to scripts and may not even have tools to read files is the wrong identity and is ahead of the correct identity segment.
    if (!isWorkflowActor) {
      sections.push(buildCliPrefixSection());
    }

    // 2. Stable agent behavior or custom prompt body
    if (hasCustomSystemPrompt) {
      sections.push(
        createSection({
          name: "Custom System Prompt",
          source: "custom_system_prompt",
          injectionTarget: "system",
          cacheHint: "stable",
          content: customSystemPrompt ? `\n${customSystemPrompt}` : "",
        }),
      );
    } else if (workflowActor !== undefined) {
      sections.push(buildWorkflowActorIdentitySection(workflowActor));
    } else {
      sections.push(buildIdentitySection(activeOutputStyle));
    }

    // 3. Dynamic system context
// custom prompt is not just replacement
// stable body, but skip the default system prompt system and systemContext; otherwise the user provides
// After custom prompt, dynamic system sections such as Session Guidance / output style will still be mixed in.
// The workflow subagent skips the three sections (desktop, Dynamic Behavior, session) oriented to "dialogue with users"
// guidance——Report outcomes faithfully has been moved to the contract), retaining memory and subsequent paragraphs.
    if (!hasCustomSystemPrompt) {
      if (!isWorkflowActor && this.config.presentationSurface === "zcode_desktop") {
        sections.push(buildDesktopContextSection());
      }

      // behaviour part right after stable sp...
      if (!isWorkflowActor) {
        sections.push(buildDynamicBehaviorSection());
      }

      // Session-specific guidance
      const sessionGuidanceSection = isWorkflowActor
        ? null
        : buildSessionGuidanceSection(
            this.config.guidanceToolNames ?? [],
            (this.config.skills?.skills.length ?? 0) > 0,
          );
      if (sessionGuidanceSection) {
        sections.push(sessionGuidanceSection);
      }

      // Memory
      if (this.config.memoryRoot) {
        const memorySection = buildMemorySection(this.config.memoryRoot);
        if (memorySection) {
          sections.push(memorySection);
        }
      }
      sections.push(buildEnvInfoSection(this.config.envInfo, this.config.model));

      // Output Style
      const outputStyleSection = buildOutputStyleSection(activeOutputStyle);
      if (outputStyleSection) {
        sections.push(outputStyleSection);
      }

      // Context Management
      sections.push(buildContextManagementSection());

      const gitSystemContextSection = buildGitSystemContextSection(this.config.envInfo);
      if (gitSystemContextSection) {
        sections.push(gitSystemContextSection);
      }
    }

    // 4. Skills appear as a meta user system-reminder, matching provider block layout.
// guidanceToolNames are current to the runtime
// Tool table; a workflow subagent that is not registered by a Skill tool is told "The following skills are available through the Skill tool",
// It will only make it believe that it has a tool that it does not have. Maintain existing behavior in absence of table (test/old caller).
    if (this.config.skills && this.skillToolAvailable()) {
      const skillsSection = buildSkillsSection({
        outcome: this.config.skills,
        metadataBudget: this.config.skillMetadataBudget,
      });
      if (skillsSection) {
        sections.push(skillsSection);
      }
    }

    // 5. Meta user context: workspace instructions/project memory first, date second.
    const requestUserContextSection = buildRequestUserContextSection({
      userInstructions: this.config.userInstructions,
      memoryIndexContent: this.config.memoryIndexContent,
      memoryRoot: this.config.memoryRoot,
    });
    if (requestUserContextSection) {
      sections.push(requestUserContextSection);
    }

    const currentDateSection = buildCurrentDateSection(this.config.currentDate);
    if (currentDateSection) {
      sections.push(currentDateSection);
    }

    // 6. Custom sections
    sections.push(...this.customSections);

    const orderedSections = orderSectionsForInjection(sections);

// Calculate total
    const totalChars = orderedSections.reduce((sum, s) => sum + s.chars, 0);
    const totalTokens = orderedSections.reduce((sum, s) => sum + s.tokens, 0);

    const systemMessages = this.assembleSystemMessages(orderedSections);
    const metaUserAttachments = this.assembleMetaUserAttachments(orderedSections);

    return {
      sections: orderedSections,
      totalChars,
      totalTokens,
      systemMessages,
      metaUserAttachments,
    };
  }

  private skillToolAvailable(): boolean {
    const names = this.config.guidanceToolNames;
    return names === undefined || names.includes(SKILL_TOOL_NAME);
  }

  private assembleSystemMessages(sections: ContextSection[]): ModelInputMessage[] {
    const messages: ModelInputMessage[] = [];

    const cliPrefixContent = buildSectionContent(
      sections.filter(
        (section) => section.injectionTarget === "system" && section.source === "cli_prefix",
      ),
    );
    if (cliPrefixContent) {
      messages.push({
        role: "system",
        content: cliPrefixContent,
        cacheControl: EPHEMERAL_CACHE_CONTROL,
      });
    }

    const stableBodyContent = buildSectionContent(
      sections.filter(
        (section) =>
          section.injectionTarget === "system" &&
          section.cacheHint === "stable" &&
          section.source !== "cli_prefix",
      ),
    );
    if (stableBodyContent) {
      messages.push({
        role: "system",
        content: stableBodyContent,
        cacheControl: EPHEMERAL_CACHE_CONTROL,
      });
    }

    const dynamicSystemContent = buildSectionContent(
      sections.filter(
        (section) => section.injectionTarget === "system" && section.cacheHint === "dynamic",
      ),
    );
    if (dynamicSystemContent) {
      messages.push({
        role: "system",
// ZCode by design: Main Agent's dynamic system block has its own left border, which is consistent for all providers.
        content: `\n\n${dynamicSystemContent}`,
        cacheControl: EPHEMERAL_CACHE_CONTROL,
      });
    }

    return messages;
  }

  private assembleMetaUserAttachments(sections: ContextSection[]): ContextMetaUserAttachment[] {
    const attachments: ContextMetaUserAttachment[] = [];

    const skillsContent = buildSkillsMetaUserBody(
      sections.filter(
        (section) => section.injectionTarget === "meta_user" && section.source === "skills",
      ),
    );
    if (skillsContent) {
      attachments.push({
        source: "skills_listing",
        content: skillsContent,
      });
    }

    const contextContent = buildContextMetaUserBody(
      sections.filter(
        (section) => section.injectionTarget === "meta_user" && section.source !== "skills",
      ),
    );
    if (contextContent) {
      attachments.push({
        source: "context_prefix",
        content: contextContent,
      });
    }

    return attachments;
  }
}

function orderSectionsForInjection(sections: ContextSection[]): ContextSection[] {
  return [
    ...sections.filter(
      (section) => section.injectionTarget === "system" && section.cacheHint === "stable",
    ),
    ...sections.filter(
      (section) => section.injectionTarget === "system" && section.cacheHint === "dynamic",
    ),
    ...sections.filter(
      (section) => section.injectionTarget === "meta_user" && section.cacheHint === "stable",
    ),
    ...sections.filter(
      (section) => section.injectionTarget === "meta_user" && section.cacheHint === "dynamic",
    ),
  ];
}

export function buildContextMetaUserBody(sections: ContextSection[]): string | null {
  if (sections.length === 0) return null;

  return [
    "As you answer the user's questions, you can use the following context:",
    buildSectionContent(sections),
    "",
    "      IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.",
  ].join("\n");
}

export function buildSkillsMetaUserBody(sections: ContextSection[]): string | null {
  const content = buildSectionContent(sections);
  if (!content) {
    return null;
  }

  return content;
}

function buildSectionContent(sections: ContextSection[]): string {
  return sections.map((section) => section.content).join("\n\n");
}

function createSection(input: {
  name: string;
  source: ContextSection["source"];
  injectionTarget: ContextSection["injectionTarget"];
  cacheHint: ContextSection["cacheHint"];
  content: string;
}): ContextSection {
  return {
    ...input,
    chars: input.content.length,
    tokens: estimateTokens(input.content),
    preview: input.content.slice(0, 100),
  };
}

// -----------------------------------------------
// Factory
// -----------------------------------------------

export function createContextBuilder(config: ContextBuilderConfig): ContextBuilder {
  return new ContextBuilder(config);
}
