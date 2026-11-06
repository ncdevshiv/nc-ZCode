import React from "react";
import { testRender } from "../../node_modules/@mbears/opentui-react/test-utils.js";
import { Sidebar } from "../../apps/zcode-cli/packages/tui/src/app-sidebar.js";
import { DEFAULT_TUI_COPY } from "../../apps/zcode-cli/packages/tui/src/app-locale.js";

const copy = DEFAULT_TUI_COPY;

const props = {
  busy: false,
  cacheStats: undefined,
  contextUsage: { contextUsed: 1234, contextWindow: 8000 },
  draft: "draft text",
  lastError: undefined,
  lastEvent: "tool finished",
  messageCount: 12,
  mcpStatus: { loading: false, servers: {} },
  mode: "build",
  model: "p/m",
  modifiedFiles: [],
  networkRequests: [],
  status: "running",
  statusDetails: ["detail one"],
  thoughtLevel: "medium",
  todos: [{ content: "todo one", status: "pending" }],
  traceId: "trace-1",
  usage: undefined,
  developerMode: true,
  version: "1.2.3",
  workspaceDirectory: "/workspace",
  workspaceGitBranch: "main",
  copy,
  subagents: undefined,
  onOpenSubagent: undefined,
  sectionExpansion: {
    subagents: true,
    apis: true,
    mcp: true,
    modifiedFiles: true,
    todos: true,
  },
  onToggleSection: () => {},
};

async function main() {
  const setup = await testRender(React.createElement(Sidebar, props as any), {
    width: 42,
    height: 60,
  });
  await setup.renderOnce();
  console.log("ZCODE_FRAME_START");
  console.log(setup.captureCharFrame());
  console.log("ZCODE_FRAME_END");
  setup.renderer.destroy();
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
