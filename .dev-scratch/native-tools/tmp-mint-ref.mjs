// Reference-value minter: runs the REAL TypeScript modules and prints the values the Rust port
// pins in its tests. Deleted after use; nothing here is executed by the test suite.
import { parse as parseYaml } from "yaml";
import {
  serializeSubagentMarkdown,
  parseSubagentMarkdown,
} from "../native-ref/subagentMarkdown.ts";
import {
  formatSubagentMarkdownModel,
  parseSubagentMarkdownSelection,
} from "@zcode/shared";
import { modelSelectionSchema } from "@zcode/shared/model-selection";

const out = {};

out.modelSelectionIssues = {
  emptyOptions: modelSelectionSchema.safeParse({ providerId: "a", modelId: "b", options: {} }).success,
  unknownKey: modelSelectionSchema.safeParse({ providerId: "a", modelId: "b", extra: 1 }).success,
  trimmed: modelSelectionSchema.parse({ providerId: "  a  ", modelId: " b " }),
  emptyReasoning: modelSelectionSchema.safeParse({
    providerId: "a",
    modelId: "b",
    options: { reasoningLevel: "  " },
  }).success,
};

out.markdownSelection = {
  plain: parseSubagentMarkdownSelection({ model: "zai-api/GLM-4.6", thoughtLevel: "high" }),
  pickerLevel: parseSubagentMarkdownSelection({ model: "zai-api/GLM-4.6$high" }),
  inherit: parseSubagentMarkdownSelection({ model: "inherit" }),
  customEncoded: parseSubagentMarkdownSelection({ model: "custom:custom%3Atenant:glm%244.6" }),
  badCustom: parseSubagentMarkdownSelection({ model: "custom:only%2Fprovider" }),
  notString: parseSubagentMarkdownSelection({ model: 42 }),
};

out.formatMarkdownModel = {
  plain: formatSubagentMarkdownModel({ providerId: "zai-api", modelId: "GLM-4.6" }),
  dollarModel: formatSubagentMarkdownModel({ providerId: "zai-api", modelId: "glm$4.6" }),
  slashProvider: formatSubagentMarkdownModel({ providerId: "a/b", modelId: "m" }),
  customPrefix: formatSubagentMarkdownModel({ providerId: "custom:x", modelId: "m" }),
};

const configs = [
  {
    name: "reviewer",
    description: "Reviews diffs",
    systemPrompt: "You review diffs.\nBe terse.",
    color: "cyan",
    tools: ["Read", "Grep", "Bash(git:*)"],
    disallowedTools: ["Write"],
    skills: ["pdf", "skill-creator"],
    permissionMode: "plan",
    maxTurns: 12,
    background: false,
    injectAgentsMd: true,
    modelSelection: { providerId: "zai-api", modelId: "GLM-4.6", options: { reasoningLevel: "high" } },
  },
  {
    name: "plain",
    description: "A: colon inside, with #hash and *star",
    systemPrompt: "body",
    mcpServers: [{ name: "demo", command: "node", args: ["a", "b"] }],
  },
  {
    name: "strings-only",
    description: "desc",
    systemPrompt: "",
    mcpServers: ["a", "b"],
  },
];

out.serialized = configs.map((config) => {
  const markdown = serializeSubagentMarkdown(config);
  const reparsed = parseSubagentMarkdown({ content: markdown, path: "/root/agents/x.md", scope: "user" });
  return { markdown, reparsed: reparsed.agent ?? reparsed.diagnostic };
});

out.yamlSamples = {
  blockFolded: parseYaml("description: >-\n  line one\n  line two\nname: x"),
  literal: parseYaml("text: |\n  one\n  two\nname: x"),
  multilinePlain: parseYaml("description: first\n  second\nname: x"),
  seqOfMaps: parseYaml("mcpServers:\n  - name: a\n    command: node\n  - name: b\n    command: py"),
  flow: parseYaml("tools: [Read, \"Write\", 'Grep']\nnumber: 12\nfloat: 1.5\nyes: true\nnil: null"),
  quotedColon: parseYaml('name: "a: b" # trailing\ndescription: \'it\'\'s here\''),
  nested: parseYaml("root:\n  child:\n    - 1\n    - 2\nother: kept"),
};

out.looseOverridesYaml = (() => {
  // The subagent reader spreads YAML first and loose second; check what a block scalar becomes.
  const frontmatter = "description: |\n  one\n  two\nname: x";
  return { yaml: parseYaml(frontmatter) };
})();

console.log(JSON.stringify(out, null, 1));
