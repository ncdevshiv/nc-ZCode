// The old default is the same as the 30 second page wait commonly used by models, and side effects such as sending are aborted before the results are read.
// The execution layer and model visible copy share this constant to avoid real timeout and tools/list description drift.
export const NODE_REPL_DEFAULT_TIMEOUT_MS = 60_000;

// MCP serverInfo was hard-coded to 0.1.0 for a long time, which was forked from the plug-in release version, causing the host to be unable to judge based on it.
//The actual loaded Browser Use runtime. This value should be synchronized with package.json during version upgrades.
// The host's own version, not the version of the browser-use plug-in. Extract the host into @zcode/node-repl-host
// This number is retained: it has always been the version announced by the node_repl server. Changing the number is equivalent to changing the protocol needlessly.
// From now on it changes with the host contract (bridge members, tool surface) and is decoupled from the respective versions of the two plugins.
//Upgraded to 0.5.0: The host contract itself has changed this time (node_repl is extracted from browser-use into an independent seed unit,
// The tool surface is adjusted according to the reconstruction and adjustment of CUA's node_repl SDK), and it should be moved according to the above rule. Numbers are the same as browser-use 0.5.0
// It's just a coincidence of homologous history and does not constitute coupling - the two are still upgraded independently.
//Up to 0.6.0: The tool surface converges to only `js`, `js_reset` and `js_add_node_module_dir` together
// moduleDirs capabilities are deleted together. The former has been a fixed return successful no-op since the fresh-kernel transformation, and "never fails"
// Will cause the model to be called repeatedly (repeated calls will continue to consume the budget); the latter
// This is to push the hosting responsibility to the model - the model cannot know which node_modules to pass on its own, all it can tell is the skill document.
// The path that the document knows can be injected by the host itself. The measured call volume of both is 0. If the host protocol changes, serverInfo must be
//Identify based on this, otherwise the host cannot distinguish which generation of tool surface it is connected to.
export const NODE_REPL_SERVER_VERSION = "0.6.0";

// The underlying capability of node_repl is general JS, but the old copy did not declare the model routing boundary, resulting in non-browser tasks
// This high-privilege tool may also be selected by mistake. Browser Use and Computer Use are legal entries, so the server and tool copywriting must be explicitly restricted.
export const NODE_REPL_SERVER_INSTRUCTIONS =
  "Browser Use and Computer Use only. Use `js` to run JavaScript in a fresh Node-backed kernel only when " +
  "the corresponding official skill instructs you to control a browser or computer. Do not use this server for unrelated tasks, " +
  "including general-purpose JavaScript, filesystem, shell, package inspection, or data processing. " +
  `Calls default to a ${NODE_REPL_DEFAULT_TIMEOUT_MS} ms timeout. ` +
  "Always provide `title` as a short user-facing description in the user's language. " +
  "Every `js` call starts fresh; reconstruct browser wrappers and recover persistent tabs from current BrowserControl facts.";

export const JS_TOOL_DESCRIPTION =
  "Browser Use and Computer Use only. Run JavaScript in a fresh Node-backed kernel with top-level await only as " +
  "instructed by the corresponding official skill to control a browser or computer. Do not use it as a general-purpose JavaScript runtime " +
  "or for filesystem, shell, package inspection, data processing, or other non-browser work. " +
  "Always provide the required `title` as a short " +
  "user-facing description in the user's language without implementation terms. If `timeout_ms` is omitted, execution times out " +
  `after ${NODE_REPL_DEFAULT_TIMEOUT_MS} ms. If the code may take more than 30000 ms including all awaited operations, you MUST set \`timeout_ms\` to at least the estimated total runtime plus 15000 ms; split the work into multiple calls if that exceeds the 120000 ms maximum. Use \`nodeRepl.cwd\`, \`nodeRepl.homeDir\`, \`nodeRepl.tmpDir\`, ` +
  "`nodeRepl.requestMeta`, `nodeRepl.setResponseMeta(meta)`, `nodeRepl.write(value)`, and " +
  "`await nodeRepl.emitImage(imageLike)`. Global bindings and module cache do not persist across calls. " +
  "For Computer Use SDK results, do not console.log/JSON.stringify the complete result or call " +
  "nodeRepl.emitImage yourself; the SDK submits the structured image/state result and you should " +
  "use nodeRepl.write for short text-only status. `get_app_state` is an explicit observation: " +
  "assign it to `const state`, always call `nodeRepl.write(state.text)`, and use " +
  "`state.state_id` plus `state.elements[*].index` for element targets; do not leave " +
  "get_app_state as the final expression or regex state_id from its text. " +
  "Metadata methods such as list_apps and list_windows may not return " +
  "action_sent; a missing field is not a failure, so inspect text or structuredContent. " +
  "Never use screenshot_display.bounds or app/window bounds as raster pixel coordinates; " +
  "x/y must be integer pixels inside the width/height of the returned raster. " +
  "Every Computer Use JavaScript call must start with the complete SDK bootstrap in the same cell " +
  "before agent.computerUse; never split bootstrap and action across calls. Never rely on agent, " +
  "runtime, browser, or imported bindings from an earlier call. " +
  "Import only `node:*` builtins and absolute `file://` URLs built from the official skill root, " +
  'for example `await import(pathToFileURL(join(root, "scripts", "client.mjs")).href)`; ' +
  "bare package specifiers do not resolve. Bootstrap the requested official capability in every call.";
