import type { Model, ModelInputMessage } from "@zcode/contracts";
import { ContextBuilder } from "../context/builder.js";
import {
  buildContextMetaUserBody,
  buildSkillsMetaUserBody,
  type ContextBuilderConfig,
  type ContextBuildResult,
  type ContextSection,
  type EnvInfo,
} from "../context/index.js";
import { buildCliPrefixSection } from "../context/sections/cli-prefix.js";
import { buildCurrentDateSection } from "../context/sections/current-date.js";
import { buildRequestUserContextSection } from "../context/sections/request-user-context.js";
import { buildSkillsSection } from "../context/sections/skills.js";
import { estimateTokens } from "../context/utils.js";
import { buildSubagentCommonNotes, buildSubagentEnvironmentContext } from "./system-prompt.js";

export interface SubagentContextBuilderConfig {
  agentPrompt: string;
  currentDate?: string;
  envInfo: EnvInfo;
  model?: Model;
  skillMetadataBudget?: number;
  skills?: ContextBuilderConfig["skills"];
  userInstructions?: ContextBuilderConfig["userInstructions"];
}

const EPHEMERAL_CACHE_CONTROL = { type: "ephemeral" as const };

export class SubagentContextBuilder extends ContextBuilder {
  private subagentConfig: SubagentContextBuilderConfig;

  constructor(config: SubagentContextBuilderConfig) {
    super(toBaseContextBuilderConfig(config));
    this.subagentConfig = config;
  }

  override setEnvInfo(envInfo: EnvInfo): this {
    this.subagentConfig = {
      ...this.subagentConfig,
      envInfo,
    };
    return this;
  }

  override build(): ContextBuildResult {
    const sections = buildSubagentContextSections(this.subagentConfig);
    const orderedSections = orderSubagentSections(sections);
    const totalChars = orderedSections.reduce((sum, section) => sum + section.chars, 0);
    const totalTokens = orderedSections.reduce((sum, section) => sum + section.tokens, 0);
    const systemMessages = orderedSections
      .filter((section) => section.injectionTarget === "system")
      .map(
        (section): ModelInputMessage => ({
          role: "system",
          content: section.content,
// The subagent context builder does not use the system assembly of main ContextBuilder.
// Still need to keep the provider cache breakpoint on each stable child system prompt.
          cacheControl: EPHEMERAL_CACHE_CONTROL,
        }),
      );
    const skillsContent = buildSkillsMetaUserBody(
      orderedSections.filter(
        (section) => section.injectionTarget === "meta_user" && section.source === "skills",
      ),
    );
    const contextContent = buildContextMetaUserBody(
      orderedSections.filter(
        (section) => section.injectionTarget === "meta_user" && section.source !== "skills",
      ),
    );

    return {
      sections: orderedSections,
      totalChars,
      totalTokens,
      systemMessages,
      metaUserAttachments: [
        ...(skillsContent
          ? [
              {
                source: "skills_listing" as const,
                content: skillsContent,
              },
            ]
          : []),
        ...(contextContent
          ? [
              {
                source: "context_prefix" as const,
                content: contextContent,
              },
            ]
          : []),
      ],
    };
  }
}

export function createSubagentContextBuilder(
  config: SubagentContextBuilderConfig,
): SubagentContextBuilder {
  return new SubagentContextBuilder(config);
}

function buildSubagentContextSections(config: SubagentContextBuilderConfig): ContextSection[] {
  const sections: ContextSection[] = [buildCliPrefixSection()];
  const agentPrompt = config.agentPrompt.trimEnd();
  if (agentPrompt) {
    sections.push(
      createSubagentSection({
        name: "Subagent Agent Prompt",
        source: "subagent_agent_prompt",
        cacheHint: "stable",
// The empty prompt is not a semantic segment, and the left boundary cannot be used as a system block alone.
// ZCode by design: Subagent agent prompt comes with a single newline left boundary relative to the CLI prefix.
        content: `\n${agentPrompt}`,
      }),
    );
  }

  sections.push(
    createSubagentSection({
      name: "Subagent Notes",
      source: "subagent_notes",
      cacheHint: "stable",
// ZCode by design: The subsequent Subagent system block will uniformly come with a double line break left border.
      content: `\n\n${buildSubagentCommonNotes()}`,
    }),
    createSubagentSection({
      name: "Subagent Environment",
      source: "subagent_environment",
      cacheHint: "dynamic",
      content: `\n\n${buildSubagentEnvironmentContext({
        agentPrompt: config.agentPrompt,
        envInfo: config.envInfo,
        model: config.model,
      })}`,
    }),
  );

  const requestUserContextSection = buildRequestUserContextSection({
    userInstructions: config.userInstructions,
  });
  if (requestUserContextSection) {
    sections.push(requestUserContextSection);
  }
  const currentDateSection = buildCurrentDateSection(config.currentDate);
  if (currentDateSection) {
    sections.push(currentDateSection);
  }
  if (config.skills) {
    const skillsSection = buildSkillsSection({
      outcome: config.skills,
      metadataBudget: config.skillMetadataBudget,
    });
    if (skillsSection) {
      sections.push(skillsSection);
    }
  }

  return sections;
}

function orderSubagentSections(sections: ContextSection[]): ContextSection[] {
  return [
    ...sections.filter((section) => section.injectionTarget === "system"),
    ...sections.filter((section) => section.injectionTarget === "meta_user"),
  ];
}

function createSubagentSection(input: {
  name: string;
  source: ContextSection["source"];
  cacheHint: ContextSection["cacheHint"];
  content: string;
}): ContextSection {
  return {
    ...input,
    injectionTarget: "system",
    chars: input.content.length,
    tokens: estimateTokens(input.content),
    preview: input.content.slice(0, 100),
  };
}

function toBaseContextBuilderConfig(config: SubagentContextBuilderConfig): ContextBuilderConfig {
  return {
    workingDirectory: config.envInfo.cwd,
    envInfo: config.envInfo,
    model: config.model,
    currentDate: config.currentDate,
    skillMetadataBudget: config.skillMetadataBudget,
    skills: config.skills,
  };
}
