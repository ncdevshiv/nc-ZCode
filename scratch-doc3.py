from pathlib import Path

p = Path('native/crates/zcode-host/README.md')
s = p.read_text(encoding='utf-8')

old = """Implemented: `v4/connection/flow`, `v4/conversation/subscribe|resync|unsubscribe`, """
new = """implemented (from 2026-10-29) **zcode-protocol (non-v4) method**: `session/debug` (session debugging ledger),
`session/compact` (manual compression, including already_running and "running rejection"), `session/goal`
(show/set/replace/pause/resume/clear + `expectedRevision` CAS), `session/subagents`
(Subagent running directory: memory registration table + subsession `subagent-run.json`, cursor paging),
`workspace/generateText` / `workspace/cancelGenerateText` (a sessionless model call and press
true cancellation of operationId). The implementation is concentrated in `src/protocol_methods.rs` (protocol layer verification/writing) +
`src/session_debug.rs`, `src/session_ttft.rs`, `src/subagent.rs` (domain facts).

Implemented: `v4/connection/flow`, `v4/conversation/subscribe|resync|unsubscribe`, """
assert old in s
s = s.replace(old, new, 1)

# tool set sentence: Task is now part of the surface
old_toolset = """Toolset Bash/Read/Write/Edit/Glob/Grep (contract includes
readOnly/destructive/concurrentSafe/sideEffectScope/timeout/maxOutput);"""
new_toolset = """Toolset Bash/Read/Write/Edit/Glob/Grep
(The contract contains readOnly/destructive/concurrentSafe/sideEffectScope/timeout/maxOutput) +
TodoWrite/TodoRead/WebFetch/ListModels/ReadImage/Skill + nc-tools native tool surface +
Connected MCP Tools + **Task Subagent Tools** (Asynchronous Tools: run a full turn of a subsession in the same runtime,
Remove Task from subsession registry to hard-disable recursion; see `src/subagent.rs` module comments and boundaries for boundaries
`specs/disabled-features.md` §3.6);"""
assert old_toolset in s
s = s.replace(old_toolset, new_toolset, 1)

# differences: refresh item 12 (tool set) and add new recorded deviations at the end of the list
old_12 = """12. The tool set is a six-tool core set; TS's ~114 handlers (bash read-only classification full list, workflow
    families, skills, node-repl, webfetch/websearch...) are not ported. """
new_12 = """12. The toolset is the core set (fs/search/bash/todo/webfetch/read-image/skill/list-models +
    nc-tools + MCP + Task); among the ~114 handlers of TS, workflow family, node-repl,ReadPdf/ReadVideo, browser/CDU family, Cron/Escalate, etc. are still not ported. What is missing is not the pile:
    For tools that are not in the registry, when the model calls it, it will get `no tool named X in this host increment`. """
assert old_12 in s
s = s.replace(old_12, new_12, 1)

old_41_tail = """ `discardSharedContext` Similarly: this host does not have a shared-context producer, and subsequent ones will only be rejected.
    Commands are equal to masks, so remain in `UNROUTED_COMMANDS`. """
if old_41_tail not in s:
    # fall back to the last numbered difference marker actually present
    old_41_tail = None

extra = """
42. **Debug ledger and TTFT facts are in-process** (same as TS): rounds/ of `session/debug`
    The records of networkEntries/cache and `v4/telemetry/local-ttft` are not saved. After restarting, the ledger will be empty.
    Start - the observation surface of TS also falls in `WeakMap`. The timestamps are all from real events (transport head,
    Injection clock, provider usage event); unmeasured preparation phase (context/hooks/persistence/
    compaction/mcp/tools/request_assembly) is **absent** in `details` and does not need to be filled in with estimated values.
43. **attempt trace is the only source of network entries**: `zcode-provider::executor::AttemptTrace` Note
    Started/ended/status/request response header/first content time/backoff duration/idle stagnation window of each physical request;
    The host does not create network facts by itself, nor does it have a second account of "number of retries".
44. **Sub-agent boundary** (see `specs/disabled-features.md` §3.6 for details): only supports the frontend
    `general-purpose`; `run_in_background` → `agent_background_unavailable`, unknown
    `subagent_type` → `agent_unknown_type`, provider missing →
    `agent_subagent_unavailable`, sub-runtime failed → `agent_child_runtime_failed`. subsession
    `taskType=subagent_child`, so it does not appear in the task list and is not restored in batches at startup.
    (skipped by `resume_sessions`), read from disk on demand by `session/subagents`.
45. **stdio distribution concurrency** (2026-10-29): Each protocol performs one task, and long model requests are no longer blocked
    `stop`/`workspace/cancelGenerateText`. Responses and frames for the same session are still represented by `Session`’s unique FIFO
    Co-order; "Response to notifications before owning them" is guaranteed by the writing order within the task. In-process caller
    (`zcode-services-agent`) is inherently concurrent and its semantics remain unchanged.
"""
if old_41_tail:
    s = s.replace(old_41_tail, old_41_tail + extra, 1)
else:
    # append before the verification section
    anchor = "## Verify"
    assert anchor in s
    s = s.replace(anchor, extra + "\n" + anchor, 1)

# integration test list: add the two new suitesold_tests = """ - Use cases for `mcp_e2e` / `mcp_tools` require `node` on PATH"""
new_tests = """- `protocol_methods_e2e`: real rounds/network entries and usage of `session/debug` (inclusive
  `hitRate` remains null when no cache usage); show/set/plan description/clear of `session/goal`;
  `session/compact` is rejected with -32010 when prompt is in progress; `workspace/generateText` is actually run once
  Model call and cancel in-flight request by operationId; Task subagent end-to-end (sub-session body +
  `session/subagents` lists success records).
- `local_ttft_e2e`: Commands with `ttft` context issue checkpoints in stages (stage timestamps are monotonic,
  observationId/commandId are consistent with the command); commands without context do not send a single fact.
- Use cases for `mcp_e2e` / `mcp_tools` require `node`""" on PATH
assert old_tests in s
s = s.replace(old_tests, new_tests, 1)
p.write_text(s, encoding='utf-8')
print('README updated', 'item42' in s)