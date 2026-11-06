# Migration Execution Plan

## Ledger inconsistency resolutions

1. Status count correction:
   - 13 capability groups, 13 status classifications (not 14).
   - PARTIAL count is 4 (agent host, tools, workflow, subagent), not 5.
   - Queue is BROKEN, not PARTIAL.

2. Tool count:
   - Original built-in tools = 40 active entries in builtInTools (including ListModels).
   - 2 are disabled boundaries (OffPeakCreate, OffPeakList).
   - Rust registers 38 tools.

3. Service member count:
   - 471 total members = 437 RPC + 6 events + 28 dynamic events.
   - 325 dispatch-evidenced + 146 boundary-removed = 471. Consistent.

4. Removed members:
   - 11 channels and 146 members are removed by documented boundaries.
   - Some are approved policy exclusions; others are unfinished migrations requiring explicit approval.

5. Duplicate/overlap:
   - Tools overlap with agent host; queue overlaps with agent host.
   - Counts are by capability group, not by unique implementation unit.

6. Missing inventory:
   - Per-symbol inventory of ~21,323 exports not completed.
   - Remaining packages not enumerated at symbol level: all listed TS packages.

## Expanded feature breakdown

### F1: Queue admission race (CAP-QUEUE)
- Original: TS command inbox busy detection
- Rust: host_queue.rs should_queue_input, turn.rs phase transition
- Status: BROKEN
- Missing: running state at admission time
- Dependencies: session phase model
- Risk: High (data integrity)
- Tests: tool_loop_e2e queued_input_drains_after_active_turn + new concurrency tests
- Acceptance: second busy sendText returns delivery=queue under all timing conditions
- Complexity: Medium

### F2: Rust code formatting (P4)
- Status: BROKEN (gate)
- Missing: formatted source
- Risk: Low
- Tests: cargo fmt --check
- Acceptance: exit 0
- Complexity: Small

### F3: Tool parity parser (P4)
- Status: BROKEN (gate)
- Missing: parser handling comma-joined entries
- Risk: Low
- Tests: pnpm native:tool-parity
- Acceptance: exit 0
- Complexity: Small

### F4: Architecture policy YAML (P4)
- Status: BROKEN (gate)
- Missing: valid YAML syntax
- Risk: Low
- Tests: pnpm architecture:check
- Acceptance: exit 0
- Complexity: Small

### F5: Service semantic parity (CAP-SERVICE-CHANNELS)
- Status: IMPLEMENTED_UNVERIFIED
- Missing: per-method semantic verification
- Dependencies: all service crates
- Risk: High
- Tests: per-method integration tests
- Acceptance: each method traced and verified
- Complexity: Large

### F6: Provider behavior (CAP-PROVIDER)
- Status: IMPLEMENTED_UNVERIFIED
- Missing: full parity verification
- Risk: Medium
- Tests: provider e2e
- Acceptance: identical request/response/error behavior
- Complexity: Medium

### F7: Built-in tools (CAP-TOOLS)
- Status: PARTIAL
- Missing: off-peak, workflow gating, schema/output/error parity
- Risk: Medium
- Tests: tool contract tests
- Acceptance: all 40 tools mapped and verified or explicitly disabled
- Complexity: Large

### F8: Workflow engine (CAP-WORKFLOW)
- Status: PARTIAL
- Missing: TS compiler front-end removal, production gating
- Risk: Medium
- Tests: workflow e2e
- Acceptance: identical workflow behavior
- Complexity: Large

### F9: Subagents (CAP-SUBAGENT)
- Status: PARTIAL
- Missing: background/profiles/permission handling
- Risk: Medium
- Tests: protocol_methods_e2e
- Acceptance: full subagent parity
- Complexity: Medium

### F10: Production desktop integration (CAP-DESKTOP)
- Status: MISSING
- Missing: startup wiring, IPC, DB/session ownership, event routing, workers, shutdown/recovery
- Risk: High
- Tests: desktop E2E
- Acceptance: desktop runs on Rust backend without TS host
- Complexity: Large

### F11: Disabled features resolution
- Status: DISABLED
- Missing: explicit approval for unfinished vs. approved exclusions
- Risk: Medium
- Acceptance: documented product decisions
- Complexity: Unknown

## Architectural blockers before production integration
1. Desktop host process is TS (desktopHostProcess.ts).
2. Database and session ownership in TS.
3. Event routing in TS.
4. Background workers in TS.
5. Provider/tool execution in TS.
6. No Rust startup wiring in desktop.

## First implementation batch (P0 only, not production cutover) — execution status
1. F1 queue admission race: code change applied in lib.rs (mark_running at admission). Validation blocked by pre-existing syntax corruption in working tree.
2. F2 cargo fmt: blocked by syntax errors in ignore.rs and other files.
3. F3 tool-parity parser: not completed.
4. F4 architecture YAML: YAML syntax already valid; architecture:check still fails due to TS boundary baseline growth.

Do not begin production desktop cutover until F1, F5, F7, and persistence verification are complete.

## Progress metrics (separate)
- Original capabilities inventoried: 13 groups (subsystem-level)
- Rust capabilities implemented: 13 groups (varying status)
- Behavioral parity verified: 2 features
- Production integration completed: 0
- Remaining known gaps: 11 features
- Remaining unverified functionality: 325 service members
