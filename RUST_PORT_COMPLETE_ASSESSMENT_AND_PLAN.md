# COMPLETE ASSESSMENT AND ACTIONABLE PLAN: Full Rust Port
**Agent:** SD-003 (System Designer) | **Board:** `.nc-tools/board-full-rust-port.md` | **Date:** 2026-11-05  
**Objective:** Port ALL business logic to Rust - every tool, every service module, CLI core/runtime, workflow compiler (replace TS compiler API dependency), all services, desktop main business logic. Everything must have tests and e2e tests. Keep plugins/skills intact.

## Executive Summary

Based on RUST-PORT-MATRIX.md (2026-09-23), the port is far from complete:
- **Rust coverage:** ~30k LOC / ~98 files / ~720 pub items vs **TS (non-UI): ~488k LOC / 2033 files / ~11.9k exports**
- **Coverage:** ~6.2% by LOC, ~4.8% by files
- **By partition:** CLI core has 6/27 partially covered, 0 fully ported; CLI runtime 2/87 partially, 0 fully; services/shared/rpc/server/provider 5/610 ported (but all 5 are blocked by dual-facade conditions), 9 partial, 596 TS-only; desktop main 0/26 ported, 4 partial, 22 TS-only
- **Critical gaps:** Protocol v4 methods (6/31 implemented), tools (12/~45 implemented), workflow compiler uses TS compiler API (11,621 LOC) with no Rust equivalent, BrowserView/Playwright (11.9k LOC) is product decision, many services have zero Rust equivalent

**No TS module can currently be legally replaced** due to dual-facade patterns (scan/watch/fsWalker) and flag-gated routing not cutting traffic to Rust.

## 1. Current State Inventory

### 1.1 Native Rust Crates (what exists)
Located in `native/crates/` (33 crates including nc-code-*, zcode-host empty):

**Core infrastructure:**
- `nc-code-core` - core types/utilities
- `nc-code-host` - host implementation (large: tools/, protocol_methods.rs, workflow/, mcp.rs, etc.)
- `nc-code-rpc` - RPC framework
- `nc-code-sidecar-proto` - sidecar protocol
- `nc-code-v4-wire` - v4 wire format
- `nc-code-proc` - process management
- `nc-code-diff`, `nc-code-scan`, `nc-code-watch`, `nc-code-image` - utilities with Rust impls
- `nc-code-provider` - provider/executor/streaming
- `nc-code-node-repl` - node repl related

**Services (Rust):**
- `nc-code-services-*` (many) - agent, automation, client, config, extensions, git, git-checkpoint, offpeak, plugins, provider, terminal, workspace, zai

**Workflow:**
- `nc-code-workflow` - has engine, journal, sandbox, scheduler, compile layer (drives TS compiler via Node kernel - NOT replaced)

**Other:** `nc-code-shell`, `nc-code-server`, `nc-code-service-core`, `nc-code-app`, `nc-code-cdp`, `nc-code-channels`

**Tests:** `cargo test --workspace` passes with 321 tests (zcode-host 77). Clippy clean, fmt clean.

### 1.2 CLI Core (apps/zcode-cli/packages/core) - TS
**95,227 LOC across 21 capability groups.** Rust coverage minimal.

**Groups:**
| Group | LOC | Rust Status | Notes |
|------|-----|-------------|-------|
| runtime | 37,015 | Partial (turn.rs + lib.rs ~1150 LOC) | 6/33 v4 methods only |
| tool | 32,166 | Partial (12 tools / 2164 LOC vs 111 handlers / 21077 LOC) | Need ~33+ more tools |
| workflow | 5,282 | None | DAG engine missing |
| subagent | 3,846 | None | |
| browser-client | 3,063 | None | |
| hooks | 2,788 | None | |
| agent | 2,295 | None | No agent tree owner |
| permission | 1,023 | Minimal (bash readonly classifier only) | No rule storage/approval gates |
| memory | 1,013 | Minimal (memory.rs 111 LOC) | |
| context | 932 | Partial (turn_context.rs 266 LOC) | |
| mcp | 920 | Partial (mcp.rs+mcp_tools.rs 539 LOC) | http/OAuth/annotations not wired |
| runtime-task | 918 | None | |
| repl | 914 | None | No JS eval host |
| compact | 863 | Partial (compact.rs 226 LOC) | |
| session-context | 523 | None | |
| plugin-reference | 498 | None | |
| system-reminder | 445 | None | |
| errors | 382 | Minimal (ToolError codes only) | |
| telemetry | 255 | None | |
| embedded-search | 71 | None | |
| model | 15 | Good (zcode-provider 7445 LOC) | executor/stream/retry/transport |

**Other CLI packages:** bootstrap (63,568), adapters (51,868), contracts (21,538), dynamic-workflow (20,034), cli (10,690), tui (8,087)

### 1.3 Tools
**TS core has ~24 tool files, total ~45 tool names needed. Rust has 12 tools. Missing critical tools:** WebSearch, Task/Agent, ApplyPatch, NodeRepl, EnterPlanMode/ExitPlanMode, AskUserQuestion, SendMessage, TaskOutput/TaskStop, CronCreate/Delete/List/Update, ReadSessionContext, SubmitResult, RespondToCoordinator, escalate, OffPeak*, entire workflow tool family (12 tools).

Tools are in `apps/zcode-cli/packages/core/src/tool/` and also referenced throughout. Need full parity.

### 1.4 CLI Runtime
**85 competency groups, 0 fully ported, 2 partially ported, 83 TS-only**
- TUI: non-visual runtime 25 groups + visual/layout 28 files (4423 LOC) + theme (341). Depends on TS-only renderer `@mbears/opentui-core`. 0% Rust.
- node-repl-host: 8 groups (1407 LOC) - closest to parity structurally (nc-code-host has mcp/stdio, nc-code-proc for process mgmt) but JS eval, per-call worker, browser/CUA injection missing.
- dynamic-workflow: 30 groups + runtime 5 groups (20034+1299 LOC). **11,621 LOC depend on TypeScript compiler API** - biggest blocker. Currently Rust drives TS compiler via Node kernel.
- telemetry (3884 LOC), i18n (1144 LOC)

### 1.5 Services/Shared/RPC/Server/Provider
**610 modules, ~4513 exports, ~154k LOC. Rust side 78 files / ~21.9k LOC (0.14:1)**
**5 RUST-PORTED, 9 RUST-PARTIAL, 596 TS-ONLY**
- Protocol gaps: v4 methods 6/31, v4 notifications 1/4, v4 command types 5/30 (rest fail-closed), legacy zcodeProtocolMethods 0/74
- TS services emit 26 v4 + 51 legacy methods; Rust implements 6 and 0
- Large TS-only: packages/provider* (8251), packages/rpc (3940), packages/client, packages/server (Rust zcode-server only 181 LOC/2 routes/stub host), conversation-share (5239)

### 1.6 Desktop Main (Electron)
**26 groups, 0 ported, 4 partial, 22 TS-only.** zcode-shell is 3 files/203 LOC vs main 47468 LOC (~0.43%)
- BrowserView/Playwright ~11900 LOC (25% of main) - Tauri has no BrowserView, **OPEN product decision**
- Auto-update ~3052 LOC (custom electron-updater provider, 4 Linux backends, Windows file lock cleanup)
- DPAPI/credentials ~1275 LOC (Windows PowerShell ProtectedData.Unprotect, macOS Keychain, Windows App-Bound needs elevated C# helper exe)
- Tray ~102 LOC (Windows-only)
- Native bridge minimal (Swift/C# stubs, zero importers)

### 1.7 Workflow Compiler - Critical Issue
**TS side** (`apps/zcode-cli/packages/dynamic-workflow/src/compiler/compile.ts`): Uses TypeScript compiler API heavily - creates TS Program, gets TypeChecker, does full type checking, has virtual compiler host, etc. ~11,621 LOC of static analysis depend on TS compiler API.
**Rust side** (`native/crates/nc-code-workflow`): Compile layer drives TS frontend through persistent Node kernel process (`compile/kernel.rs`). Not a replacement - it's a bridge.

**Need to replace TS compiler API dependency entirely with Rust implementation.** Options:
1. Implement a TypeScript parser + type checker in Rust (extremely complex - would need to be TS-compatible or define subset)
2. Use existing Rust TS parsers (swc, oxc, biome) and build type checking on top - still massive effort
3. Redesign workflow DSL to not need full TS type checking - but need to understand product requirements
4. Keep compatibility layer but move to Rust-native parsing/checking

This needs careful analysis. The workflow is central to the system.

## 2. Dependencies & Blockers

### 2.1 Blocking dependencies (must do in order per matrix)
1. **Host default flip** - Complete v4 methods (need +25, have 6) and v4 commands (+25, have 5), add all 33 missing tools. Only then can `ZCODE_NATIVE_HOST` default to ON.
2. **JS tool deletion** - Once host carries traffic, facade calls must be cut over so TS dual-path impl becomes dead code.
3. **First legal old.back move** - Requires true cutover (flag on -> Rust, flag off -> doesn't exist). Currently all are dual-facade.
4. **Workflow engine** - Need to decide how to handle static analysis without TS compiler API (11,621 LOC). Otherwise stays TS-only.
5. **Electron -> Rust shell** - BrowserView decision needed (product decision). ~25% of main.

### 2.2 Dual-facade issues (cannot delete TS yet)
- `fsWalker.ts` / `fileWatcherService.ts` - fall back to TS if native returns null/fails; `inProcessScanRunner.ts`, `fsCleaner.ts` import them
- `wire-binary.ts`, `schema-v1.ts`, `DELIVERY_PROFILES` - no production traffic cutover, flags don't route traffic
- All 5 "1:1 candidates" fail conditions 3 or 4

### 2.3 Feature flags
- `ZCODE_NATIVE_HOST` defaults OFF (zcodeAgentProcessManager.ts:357,368) - host not carrying production traffic
- `ZCODE_NATIVE_SIDECARS` defaults ON for diff/image/scan/watch; proc defaults off (config.ts:44)

## 3. Detailed Porting Requirements

### 3.1 Every Tool (CLI Core Tools)
Need to port all tools from `apps/zcode-cli/packages/core/src/tool/` and any other tool definitions. Rust tools likely in `nc-code-host/src/tools/`. Need full parity - schemas, validation, execution, error handling, permissions, etc. Must have unit tests for each tool.

### 3.2 CLI Core Runtime
Port everything in core/src/ not just tools: runtime (turn management), agent orchestration, subagents, workflow integration, permission system, memory, context management, hooks, browser-client, runtime-task, repl, compact, session-context, plugin-reference, system-reminder, errors, telemetry, embedded-search, model integration. All business logic.

### 3.3 CLI Runtime (TUI, node-repl, workflow, telemetry, i18n)
- **TUI**: This is complex - depends on `@mbears/opentui-core`. May need to either port the renderer bindings or keep TUI in TS? But "all business logic" - UI rendering logic vs business logic needs careful separation. TUI has non-visual runtime (business logic) vs visual/layout components. Need to port business logic.
- **node-repl-host**: Port JS eval host, per-call workers, browser/CUA injection
- **dynamic-workflow**: Full engine + runtime + **replace TS compiler API** with Rust implementation
- **telemetry, i18n**: Full port

### 3.4 All Services Modules
596 TS-only service modules need Rust equivalents. This includes all of packages/services/src/*, packages/provider*, packages/rpc, packages/client, packages/server business logic, conversation-share, etc. Everything that is business logic (not just UI).

### 3.5 Desktop Main Business Logic
Port business logic from packages/desktop/src/main/* where it's not pure UI/renderer concerns. But BrowserView is a big question - Tauri doesn't have BrowserView equivalent. Also need to understand if moving to Rust shell means using Tauri - there's nc-code-shell crate. Need to assess.

### 3.6 Protocol Layer
Complete v4 protocol implementation - all 31 methods, 4 notifications, 30 command types. Eliminate legacy protocol dependency. This is critical for host to take over.

### 3.7 Workflow Compiler - Replace TS Compiler API
This is the hardest part. Need to:
1. Understand full compiler pipeline: parse, type check, site analysis, schema synthesis, lowering, etc.
2. Either implement TS-compatible checker or redesign
3. May need to use swc/oxc for parsing/transforming, build type checking
4. Must maintain exact same behavior/diagnostics
5. All tests must pass

## 4. Testing Requirements
"Everything must have tests and e2e tests"
- **Unit tests** for every module, function, tool in Rust (cargo test)
- **Integration tests** for cross-module functionality
- **E2E tests** - need to understand existing e2e test patterns. Look for e2e test setup.
- **Golden tests** - there are golden vectors (bun run golden:vectors/check) - need to maintain compatibility
- **Parity tests** - ensure Rust impl matches TS behavior
- **All existing tests must continue to pass**

## 5. Phased Action Plan

### Phase 0: Assessment & Planning (CURRENT - SD-003)
**Status:** In Progress  
**Tasks:**
- [x] Register on board
- [x] Read RUST-PORT-MATRIX
- [x] Explore native crates
- [x] Understand TS structure
- [ ] Create detailed assessment document (this doc)
- [ ] Inventory every file that needs porting
- [ ] Create dependency graph
- [ ] Estimate effort

### Phase 1: Foundation & Protocol Completion
**Goal:** Make Rust host capable of carrying traffic  
**Dependencies:** None  
**Estimated scope:** Large

**Tasks:**
1. **Complete v4 protocol** - Implement all missing v4 methods (25 more), notifications (3 more), command types (25 more). Make fail-closed paths pass. Update protocol types.
2. **Complete tool implementations** - Port all missing tools to reach parity (~33+ tools). Each needs schema, validation, execution, tests.
3. **Complete host runtime core** - Port missing pieces of turn/runtime/agent/subagent/orchestration needed for tools.
4. **Permission system** - Full rule storage, approval gates, policy engine in Rust
5. **Session management** - Ensure full parity with TS session handling
6. **Tests:** Unit tests for all new code, integration tests for protocol/tools

### Phase 2: CLI Core Business Logic Port
**Goal:** Port all non-tool business logic in core packages  
**Dependencies:** Phase 1

**Tasks:**
1. Port workflow integration (or prepare for new compiler)
2. Port hooks, browser-client, runtime-task, repl logic
3. Port memory, context, compact, session-context
4. Port plugin-reference, system-reminder, telemetry integration
5. Port MCP fully (http/OAuth/annotations)
6. All error handling, utilities
7. Tests for everything

### Phase 3: Workflow Compiler Replacement (Critical)
**Goal:** Replace TS compiler API dependency with Rust implementation  
**Dependencies:** Phase 1-2 understanding  
**This is the hardest part**

**Tasks:**
1. **Deep analysis** of TS compiler usage in dynamic-workflow
   - Understand full pipeline: parse/wrap, type check, site extraction, schema synthesis, lowering, analysis passes
   - Document all TS compiler API calls and what they do
   - Understand facade system, virtual host, diagnostics
2. **Design Rust compiler architecture**
   - Parser: use swc or oxc (both are Rust, fast, TS/JS capable). Need to decide.
   - Type checking: This is the hard part. Options: (a) implement minimal type checker for workflow DSL subset, (b) use existing type checker, (c) embed/reskin approach, (d) transpile to different model
   - Site analysis, schema synthesis, lowering need to be reimplemented to match exactly
3. **Implement incrementally**
   - Start with parsing/wrapping equivalent
   - Implement type checking for the constrained workflow subset
   - Port all analysis passes
   - Port lowering
   - Port diagnostics/rewrites
4. **Parity testing** - Must produce identical results to TS compiler on all existing workflow tests
5. **Replace the kernel bridge** with native Rust implementation
6. **Tests:** Extensive unit tests for each compiler phase, golden tests for compilation, e2e workflow tests

**Note:** Need to look up specs/workflow-capability-boundary.md and understand the full picture.

### Phase 4: CLI Runtime Port
**Goal:** Port TUI runtime logic, node-repl, telemetry, i18n  
**Dependencies:** Phase 1-3

**Tasks:**
1. **TUI business logic** - Separate business logic from rendering. Port session/mode/model/tool/todo/permission/checkpoint state management, event handling, etc. Visual rendering may need different approach - but "all business logic" must be ported.
2. **node-repl-host** - Full port including JS eval, workers, browser/CUA injection
3. **telemetry** - Full port
4. **i18n** - Full port
5. **Tests:** Unit and integration tests

### Phase 5: Services Port (Bulk)
**Goal:** Port all 596 TS-only service modules  
**Dependencies:** Phase 1-4 for core dependencies

**This is the largest volume. Need systematic approach.**

**Tasks:**
1. **Inventory all services** - Catalog every module in packages/services/src, packages/provider*, packages/rpc, packages/client, packages/server
2. **Port by dependency order** - Shared/protocol first, then core services, then dependent services
3. **Maintain API compatibility** - Or update all call sites
4. **Each service needs full implementation + tests**
5. **Focus on business logic** - Not UI-specific code

### Phase 6: Desktop Main Port
**Goal:** Port desktop main business logic  
**Dependencies:** Phase 1-5

**Tasks:**
1. **Assess BrowserView** - This is product decision per matrix. Need to understand if it's in scope. "Port ALL business logic" - BrowserView is core functionality (11.9k LOC). But Tauri can't do it. Options: (a) keep Electron parts, (b) find alternative, (c) extend Tauri. Need to clarify with requirements.
2. **Port auto-update** logic if possible
3. **Port credentials/DPAPI** - complex platform-specific code, need Rust equivalents (keyring crates, etc.)
4. **Port tray, window management business logic**
5. **Port process management, IPC**
6. **nc-code-shell exists but minimal** - need to extend significantly or take different approach

### Phase 7: Cutover & Elimination
**Goal:** Make Rust the default, eliminate TS dual paths  
**Dependencies:** All previous phases

**Tasks:**
1. **Flip flags** - Make `ZCODE_NATIVE_HOST` default ON once all gaps closed
2. **Cut over traffic** - Change facades to prefer Rust, remove fallbacks where appropriate
3. **Delete dead TS code** - Follow old.back/README.md conditions strictly (flag off path must not exist). Only delete when truly safe.
4. **Update all imports/references**
5. **Integration testing** - Full system tests
6. **E2E tests** - Comprehensive e2e coverage

### Phase 8: Testing & Verification
**Goal:** Ensure everything has tests and e2e tests  
**Ongoing throughout, final verification here**

**Tasks:**
1. **Unit test coverage** - Every Rust module needs tests
2. **Integration tests** - Cross-component tests
3. **E2E tests** - End-to-end scenarios for all major features
4. **Parity tests** - Compare behavior with TS where both exist during transition
5. **Golden tests** - Update and verify
6. **Run all checks:** `cargo test --workspace`, `cargo clippy`, `cargo fmt`, `bun run typecheck`, `bun run lint`, `bun run architecture:check`
7. **Performance testing**
8. **Security review**

## 6. Implementation Strategy

### 6.1 Principles
- **Spec-first:** Update specs before changing behavior (per AGENTS.md)
- **No mocks/fakes/stubs** - Everything must be real implementation
- **Tests first where possible** - Especially for bug fixes
- **Preserve behavior exactly** - Parity is critical
- **Keep plugins/skills intact** - As specified
- **Follow existing patterns** - Rust codebase style, TS patterns
- **Delete over add** - Smallest working diff

### 6.2 Tools & Crates to Use
- **Parsing:** swc or oxc for JS/TS parsing (both mature Rust crates)
- **Async:** tokio
- **Serialization:** serde
- **Error handling:** thiserror/anyhow as appropriate
- **Testing:** standard Rust test framework
- **For workflows:** May need to build custom type checker or use existing solutions

### 6.3 File Organization
- Follow existing native/crates structure
- Add new crates as needed
- Mirror module structure where sensible
- Keep tests in `tests/` directories

### 6.4 Migration Approach
- **Strangler pattern:** Add Rust implementations alongside TS, route traffic via flags, eventually remove TS
- **But need to follow the rules** - can't delete TS until conditions met
- **Feature flags are key** - need to wire them properly
- **Incremental** - Port module by module, test as we go

## 7. Critical Decisions Needed

1. **Workflow compiler replacement strategy** - How to replace TS compiler API? This is make-or-break.
2. **BrowserView in desktop** - Product decision. Without it, can't fully replace Electron main.
3. **TUI rendering** - Keep TS renderer or port? "All business logic" must move; rendering might be treated differently.
4. **Approach for 596 service modules** - Systematic porting vs rearchitecture?
5. **Testing strategy for e2e** - How to structure e2e tests in Rust?

## 8. Immediate Next Steps

1. **Deep dive into workflow compiler** - Understand every use of TS compiler API, design replacement
2. **Complete protocol v4** - Get host to full v4 parity
3. **Complete critical tools** - Port tools needed for basic functionality
4. **Set up testing infrastructure** - Ensure we can write/run tests properly
5. **Create detailed inventory** - Script or manually catalog every TS file that needs porting
6. **Get team alignment** - This is massive, needs coordination

## 9. Risks & Mitigations

| Risk | Impact | Likelihood | Mitigation |
|------|--------|------------|------------|
| Workflow compiler replacement too complex | Critical | High | Start with deep analysis, consider subset first, may need hybrid approach |
| BrowserView can't be ported | High | High | Need product decision, may need to keep Electron for this |
| Breaking changes | High | High | Extensive testing, parity tests, gradual cutover |
| Missing behavior details | High | High | Read specs, tests, existing behavior carefully |
| Timeline/scope creep | High | High | Phased approach, focus on business logic first |
| Plugin/skill breakage | Medium | Medium | Keep them intact as required, test them |

## Conclusion

This is a massive undertaking - porting ~450k LOC of business logic from TS to Rust. The matrix shows it's ~6% complete. Key blockers are workflow compiler (TS compiler API replacement) and protocol/tool completion. Desktop has BrowserView as potential product blocker.

We need to be systematic, follow all rules, write tests for everything, and not delete TS code until cutover conditions are met. The plan above is comprehensive but will need refinement as we discover details.

**SD-003 assessment complete. Ready to proceed with Phase 1 upon orchestrator approval.**
