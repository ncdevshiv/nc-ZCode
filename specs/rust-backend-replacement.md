# Spec: Rust backend replacement (TS service channels → Rust channel server)

> **WARNING**: Never trust docs/logs without fully auditing code. Always verify against actual implementation.
Status: accepted 2026-10-27. M1–M3 landed; M4–M7 in execution.
Z.ai/ZCode boundary: `specs/zai-claim-boundary.md` (updated 2026-11-05 - all ZCode services removed; every provider still works by default). **Always audit code, not just docs.**
Date: 2026-10-27
Owner: native backend
Governing specs: `specs/rust-native-crates.md` (crate map / dependency direction; this spec adds to it), `specs/typescript-frontend-only.md` (language boundary ratchet), `specs/rust-pure-native-roadmap.md` (phase definitions). On conflict: crates/directions follow this spec + `rust-native-crates.md` additions; language boundary follows `typescript-frontend-only.md`.

## 0. Goal and definition of done

**Goal**: the current frontend (`packages/web`, and the future Tauri window) changes no calling convention, while the backend (HTTP bootstrap, WebSocket channel RPC, all 40 service channels) is implemented in Rust. The Tauri application loads the **current frontend build** and connects to the **in-process Rust backend**.

**Definition of done** (every item is required):

1. Every channel in `native/fixtures/service-channels.json` is registered in the Rust registry, and every `role = "rpc"` member has a real implementation (no `todo!()`, no always-true / always-empty stub, no "hook it up later" branch).
2. Every `role = "event"` / `"dynamic-event"` member has a real event source: events come from real state changes (file watching, session frames, process sampling), not from a timer faking them.
3. Frontend boot and the core flows (open workspace → session list → send message → tool call → streaming frames → stop → restart recovery) run through inside a **real Tauri window**, with no unknown channel/method in the backend logs.
4. For capabilities that cannot exist in this product (currently: the CUA placeholder package and the "remote connection" family), the Rust side returns the same fail-closed result as TS and a test pins it — this is a product fact, not a stub.
5. The TS backend entry points (`packages/server`'s HTTP/WS startup path, `createLocalServices`' runtime assembly) are deleted or moved into `old.back/` after the frontend switches to Rust completely, and the `typescript-frontend-only` ratchet tightens in step.

**Machine definition for done items 1–2**: `pnpm native:coverage` (`native/tools/check-channel-coverage.mjs`; non-zero = not done). It places every member name of `service-channels.json` into its channel using **dispatch evidence** — resolving dispatch range via `registry.register("x", self.field)` → field type → `impl Channel for Type`, then searching for `"name" =>` / `"name" |` / `== != "name"` / `.insert("name"`; same-named members (`list`, `get`, `create`) cannot borrow another channel's dispatch. A removed member counts only with a **spec-cited reason** in `CHANNEL_BOUNDARY` / `MEMBER_BOUNDARY`, otherwise it errors; an entry in the boundary list not in the TS table is also an error; a registered channel with no evidence is an error (not 0).

**Non-goals**: rewriting the frontend UI; Electron→Tauri platform capabilities (dialogs / updates / deep links / BrowserView) advance under the R4 bucket of `rust-native-crates.md`. This spec only requires the minimal shell surface needed to run.

## 1. Current-state evidence (measured 2026-10-27)

| Area | Files | Lines |
| -------------------------- | -----: | --------: |
| `packages/rpc/src`         |     16 |     3,295 |
| `packages/services/src`    |    298 |    87,469 |
| `packages/server/src`      |     48 |    10,771 |
| `packages/client/src`      |      6 |       723 |
| `packages/desktop/src/main`|    188 |    47,450 |
| `packages/web/src` (frontend) |     16 |     2,875 |

- Service-surface inventory (machine generated, manual edits forbidden): `node native/tools/inventory-service-channels.mjs` → `native/fixtures/service-channels.json`. Current: **40 channels / 437 rpc / 6 events / 28 dynamic events** (471 members in total).
- Web startup contract (`packages/web/src/main.tsx`): 1) `GET /api/server-info` → `ServerRemoteInfo` (`workspaces[0].path/workspaceIdentity` optional); 2) `new WebSocket(ws[s]://<origin>/ws)`, binary frames; 3) `SocketProtocol` → `ChannelClient` → `RemoteServiceAccess` (`ProxyChannel.toService`).
- Channel wire (already ported byte-for-byte in `native/crates/nc-code-rpc` and passing golden): 13-byte frame header + `[type,id,channel,method] + arg`, `RequestType` 100–103, `ResponseType` 200–204. Golden is produced by driving the real `ChannelClient`/`ChannelServer`.
- **Argument shape**: `ProxyChannel.toService`'s `call` always sends a **positional argument array** (`channel.call(propKey, [...args])`). Server handler receives `WireValue::Array` and decodes in order; event listener arg is `undefined` (plain) or call args (`onDynamicXxx(arg)`).
- Event naming (`proxy-channel.ts`): `on[A-Z]…` = plain event (`listen(name)`); `onDynamic[A-Z]…` = dynamic event (`listen(name, arg)`); everything else is RPC. Unknown method throws `Method not found: <name>`; unknown event throws `Event not found: <name>`.

## 2. Target architecture

```
┌────────────────────────────────── Tauri window (WebView2/WKWebView) ───────────────────────────────────┐
│ packages/web build output (unchanged)                                                                  │
│    GET /api/server-info      WS /ws (binary frames)                                                    │
└───────────────▲──────────────────────────▲─────────────────────────────────────────────────────────────┘
                │ HTTP                     │ channel RPC
┌───────────────┴──────────────────────────┴─────────────────────────────────────────────────────────────┐
│ nc-code-shell (Tauri 2): setup starts the embedded server (127.0.0.1:0), window navigates to that origin │
└───────────────▲────────────────────────────────────────────────────────────────────────────────────────┘
                │
┌───────────────┴────────────────────────────────────────────────────────────────────────────────────────┐
│ nc-code-server (axum): /healthz, /api/server-info, /ws upgrade, static assets + SPA fallback             │
└───────────────▲────────────────────────────────────────────────────────────────────────────────────────┘
                │ Registry<Channel>
┌───────────────┴────────────────────────────────────────────────────────────────────────────────────────┐
│ nc-code-app (composition root): data dir, settings, workspace, registry wiring, lifecycle                │
│   ├─ nc-code-services-config     setting/broadcast/client-config/…                                       │
│   ├─ nc-code-services-workspace  file/git/terminal/watcher/system/…                                      │
│   ├─ nc-code-services-agent      zcode-agent/zcode-session/zcode-task/window-controller                  │
│   └─ nc-code-services-ecosystem  skills/plugins/mcp/subagents/commands/hooks/memory/…                    │
└───────────────▲────────────────────────────────────────────────────────────────────────────────────────┘
                │ in-process
┌───────────────┴────────────────────────────────────────────────────────────────────────────────────────┐
│ nc-code-host (already exists): v4 conversation runtime, tool loop, goal, workspace hooks, MCP            │
│ nc-code-core/provider/watch/proc/image/diff/scan: existing native capabilities                           │
└────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

### 2.1 New crates and dependency direction

| crate | Responsibility | Allowed dependencies |
| ------------------------- | -------------------------------------------------------------------- | -------- |
| `nc-code-rpc` (already exists) | channel wire encode/decode, frames, golden contract | serde/serde_json |
| `nc-code-channels` (new) | connection runtime: registry, Initialize, call/listen/cancel, EventFire forwarding | `nc-code-rpc`, tokio |
| `nc-code-service-core` (new) | shared service-layer pieces: positional argument decoding, typed JSON, ServiceError → RpcErrorPayload, path/atomic write, event bus | `nc-code-rpc`, serde, tokio |
| `nc-code-services-config` (new) | config/credential/account/usage family channels | `nc-code-service-core`, `nc-code-channels` |
| `nc-code-services-workspace` (new) | file/git/terminal/watcher/system family channels | same as above + `nc-code-watch`, `nc-code-proc`, `nc-code-scan`, `nc-code-image`, `nc-code-diff` |
| `nc-code-services-agent` (new) | agent/session/task/window-controller | same as above + `nc-code-host`, `nc-code-provider` |
| `nc-code-services-ecosystem` (new) | skills/plugins/mcp/subagents/commands/hooks/memory/share/feedback/sync | same as above |
| `nc-code-app` (new) | composition root: paths, config loading, registry assembly, disposer | all services crates |
| `nc-code-server` (already exists) | axum HTTP/WS/static; `serve(app, options)` is embeddable | `nc-code-app`, `nc-code-channels`, `nc-code-rpc` |
| `nc-code-shell` (already exists) | Tauri: start the server, navigate the window | `nc-code-app`, `nc-code-server` |

**Forbidden**: a service crate depending on `nc-code-server`/`nc-code-shell`; `nc-code-channels` depending on any service crate; circular dependencies. A new edge must first amend this table, then be written in code.

### 2.2 Ownership and event order

- **Each connection** owns its channel context (clientMode, connectionId); service instances are process-level singletons and so is the event bus; subscriptions are connection-level.
- **Event order**: events of the same channel keep commit order (one ordered sender per broadcast). Responses and events share one FIFO outbound queue; no "response first, then an earlier event".
- **Cancellation**: `PromiseCancel` cancels only that request's token; `EventDispose` unsubscribes only that id. Long tasks stop writing events once cancelled.
- **Idempotence**: repeated `EventDispose`/`PromiseCancel` are no-ops; a response after cancellation is discarded.

## 3. Channel runtime contract (`nc-code-channels`)

- Connection established → send `Initialize` immediately (`ResponseType.Initialize`), matching TS `ChannelServer` (ready at construction).
- Frame intake: `FrameDecoder` tolerates fragmentation/sticky packets; only `Regular` frames accepted as channel messages; non-Regular ignored by type (PersistentProtocol semantics added later).
- Requests:
  - `Promise`: decode header (4-tuple) and body; `arg` handed to channel as-is (service layer decodes as array).
  - `EventListen`: plain events ignore arg; dynamic events pass arg to channel.
  - Unknown channel: return `PromiseError{name:"Unknown channel"}` **immediately** (TS times out after 1s; this host is fail-fast so a "missed-ported channel" is not hidden). `EventListen` on unknown channel dropped outright (TS drops after timeout too).
  - Unknown method: `PromiseError{name:"Error", message:"Method not found: <name>"}`.
  - Handler `Err(ServiceError)` → `PromiseError`; `Err(Thrown)` non-Error → `PromiseErrorObj` (aligned with TS non-Error branch).
- Events: channel subscription produces `WireValue` stream; connection sends `EventFire{id}` per value. On `EventDispose`/close the task is dropped and must not leak.
- Frame ceiling 64 MiB (`nc-code-rpc` default); exceeding it disconnects and records a diagnostic.
- Codec uses `WireValue` only: top-level bytes via Buffer/VSBuffer, nested bytes via `__zcode_rpc_nested_uint8array_v1` marker (`serialization::nested_uint8array_*`).

## 4. Service migration matrix (grouped by channel)

> Status: `—` not started; `W` wired and tested; `F` product unavailable, fail-closed alignment (with evidence); `R` removed from Rust backend per `specs/zai-claim-boundary.md` (not registered, not ported). **The common model capabilities of any provider are not affected.** Every completed channel updates this table with test location; **The table is the release threshold, not a plan**.

### 4.1 Configuration/Account Family (`nc-code-services-config`, 13 channels / ~110 rpc)

| channel | rpc | events | status | dependencies/notes |
| --- | ---: | ---: | --- | --- |
| setting | 4 | 0 | W | `nc-code-services-config::setting`; schema/defaults generated by real Zod; test 17 |
| broadcast | 5 | 1 | W | `nc-code-services-config::broadcast`; claim state machine + event; test 3 |
| client-config | 1 | 0 | W/D | `nc-code-services-client`; **Remote reading is disabled** (`specs/disabled-features.md` §2.3), local returns `{pluginStoreOrder:null}` |
| client-scenes | 1 | 0 | W/D | `nc-code-services-client`; **Remote reading is disabled** (`specs/disabled-features.md` §2.4), local returns empty directory envelope |
| onboarding-record | 8 | 0 | W | `nc-code-services-config::onboarding`; file reading/writing/claiming/backfilling; test 3 |
| settings-sync | 6 | 0 | W/D | `nc-code-services-settings-sync`: **W** for discovery + import (skills/commands/plugins copy into `~/.zcode/{skills,commands,plugins}` or `<workspace>/.zcode/...`, MCP merged into `mcp.servers`, plugin dirs registered in `plugins.dirs`, `targetExists` skip); **D** for remote upload (`getStatus → {enabled:false}`, `specs/disabled-features.md` §2.8). Recorded difference: `importMode` copies instead of the TS `symlink` default. Tests: 21 unit + 2 integration (real detect→import→idempotent) |
| provider-settings | 15 | 1 | W | `nc-code-services-provider`; personal provider/model store, templates, CRUD, resolve, real connectivity detection; 8 tests |
| model-selection | 2 | 1 | W | `nc-code-services-provider`; projected candidate by personal store; empty view consistent with TS boot fixture field by field |
| provider-provisioning-target | 1 | 0 | F/W | Trusted host only; writable by local machine, rejected by web client |
| credential | 3 | 0 | W | `nc-code-services-config::credential`; AES-256-GCM (TS ciphertext fixture decryptable); test 5 |
| zai-claim (new) | 3 | 0 | R | **Removed** (all ZCode services removed per `specs/zai-claim-boundary.md` updated 2026-11-05) |
| oauth | 13 | 0 | R | **Removed** (all ZCode services removed per `specs/zai-claim-boundary.md` updated 2026-11-05) |
| usage-stats | 8 | 0 | R | **Remove** (remote usage/limit query, not claim data) |
| coding-plan-subscription | 4 + 25 removed | 0 | R | **Removed** (all ZCode services removed per `specs/zai-claim-boundary.md` updated 2026-11-05) |

### 4.2 Workspace family (`nc-code-services-workspace`, 8 channels / ~68 rpc)

| channel | rpc | events | status | dependencies/notes |
| --- | ---: | ---: | --- | --- |
| file | 17 | 0 | W | `nc-code-services-workspace::file`; read-write/scope/binary/preview, workspace creation, index chunking, `.zcodeignore` read-write/sync/reset; 11 tests |
| media-preview | 3 | 0 | R | **Removed** (overridden by file.readMediaPreview; standalone channels are no longer registered) |
| file-watcher | 4 | 1 | W | `nc-code-services-workspace::watcher`; notify 8, 150ms anti-shake, single-path transparent transmission, unwatch/disposeAll; 12 tests |
| git | 18 | 0 | W | `nc-code-services-git::channel` (all 18 are transplanted according to the scope of `gitService.ts`); subprocess git, args array disabled shell; test 45 |
| git-checkpoint | 4 | 0 | W | `nc-code-services-git-checkpoint` (repo/store/service/helpers layer corresponds to TS one-to-one): temporary `GIT_INDEX_FILE` → write-tree/commit-tree → hidden ref; restored after three-party conflict verification (missing/unexpected/type/content); test 2 real git warehouses e2e (round-trip+conflict+force, guard and damage manifest fail-closed) |
| terminal | 6 | 0 | W | `nc-code-services-terminal` (portable-pty 0.9 ConPTY); single test 46 + channel e2e 6 + pty e2e 5 all green (ENTRY 103: ConPTY sends `ESC[6n` when starting, the test end must return the cursor position report; at the same time, the pump thread blocking when the child process exits causes `onDynamicExit` deadlock) |
| system | 3 | 0 | W | `nc-code-services-workspace`; info/terminal shell probe/intranet TCP+HTTP probe; 4 tests |
| window-controller | 8 | 0 | W | `nc-code-services-client::window_controller`; only routed to `zcode-task` facade (single source of truth), controller frames are pushed by topic |

### 4.3 Agent/session family (`nc-code-services-agent`, 3 channels / ~196 rpc + 16 events)

| channel | rpc | events | status | dependencies/notes |
| --- | ---: | ---: | --- | --- || zcode-agent | 100 | 16 | W | 100 rpc All have distribution points (`pnpm native:coverage` counts). Core: in-process `nc-code-host` (initialize, create/send/read/close/list, v4 subscribe/resync/unsubscribe, v4 command/query, rowsRange, fileChanges, rewindPreview, hello/init/flow, runtime prefs/identity, frame/lifecycle events); 5 mirror members shared with zcode-session `build_snapshot`/`run_config_command` single write path; plugin 18 members mirrored to `plugin-management`; automation 10 members + saved workflow 5 members; attachment upload/read surface (the six `attachment*V4` members Begin/Chunk/Commit/Abort/Read/PreviewSource) routed to the host transaction, and `sendPrompt` maps composer attachments (localPath → zero-upload path ref, inline bytes → content-addressed `zcode-artifact://` ref); 2026-10-29 Complete session reading surface/control surface `readSessionDebug`, `listSessionSubagents`, `compactSession`, `goalSession`, `generateWorkspaceText` and event `onDynamicLocalTtftFacts` (see `native/crates/nc-code-host/src/protocol_methods.rs`, `session_debug.rs`, `session_ttft.rs`, `subagent.rs` for implementation). Only `onDynamicSessionEvent` is not wired according to the F evidence in §4.5 (TS has been abandoned, and this host has no second event source). |
| zcode-session | 15 | 0 | — | Session application service |
| zcode-task | 65 | 0 | W | `nc-code-services-agent::task`; 59 rpc + 6 events all have distribution points (`pnpm native:coverage` is counted), method-level tests are still completed according to §6. Claude Code native history import is wired end to end (`scanImportableClaudeSessions` + `importClaudeSessions` → `session/import`), see `specs/claude-session-import.md` |

### 4.4 Ecosystem (`nc-code-services-ecosystem`, 16 channels / ~106 rpc + 0 events)

| channel | rpc | status | remarks |
| --- | ---: | --- | --- |
| commands | 6 | W | `nc-code-services-extensions::commands`, test including write/disable/rename/delete full loop |
| memory | 2 | W | `nc-code-services-extensions::memory`, real reading and writing of the project memory directory |
| skills | 6 | W | `nc-code-services-extensions::skills` (compiled for the first time after completing the `mod skills` statement in this round) |
| hooks | 3 | W | `nc-code-services-extensions::hooks`; reuse host trust store/admission, no second writing path |
| subagents | 8 | W | `nc-code-services-extensions::subagents`; session subagent registration interface |
| plugins | 7 | W | `nc-code-services-plugins::service`; The TS product itself has retired this channel (empty overview + `supported:false`, changing members throws an error), Rust mirroring one by one = parity |
| plugin-management | 18 | — | Same as above; UI takes the progress event of this channel || off-peak-task | 11 | D | Domain layer intact (repo/client/runtime/service, 22 tests) but **product disabled cloud scheduling**: register `DisabledOffPeakTaskService` (support=false/feature_disabled, `specs/disabled-features.md` §2.1) |
| plugin-sync | 7 | R | `specs/zai-claim-boundary.md` (remote synchronization) |
| skill-sync | 5 | R | Same as above |
| mcp-sync | 8 | R | Same as above |
| prompt-attachment-transfer | 5 | R | Same as above |
| settings-sync | 6 | R | See §4.1 |
| conversation-share | 9 | R | `specs/zai-claim-boundary.md` |
| feedback | 15 | R | Same as above |
| usage-stats | 8 | R | See §4.1 |
| cua-permission | 2 | F | Product not available, fail-closed (§4.4) |
| cua-pip-session | 3 | F | Same as above |

### 4.4.1 `zcode-session` (15 rpc = 11 `W` + 4 `F`): registered

UI side `packages/ui/src/hooks/useActiveTaskSnapshotMeta.ts:66` with
`readSession` of `packages/ui/src/v4/SessionPane.tsx:1886` now reaches the real implementation
(Previously `Unknown channel` + `.catch()` silently downgraded).

| Status | Members (11 `W`) |
| --- | --- |
| W | initializeWorkspace, getWorkspaceRuntimeIdentity, createSession, resumeSession, listSessions, readSession, readSessionMessages, closeSession, setModel, setThoughtLevel, setMode |

- **Snapshot construction**: only `AgentService::build_snapshot` → `snapshot::build` (strictly `zcodeSessionStateSnapshotSchema`), input = host session fact + v4 snapshot + provider model facts (`settings_facts` field-aligned `toModelOption`, `provider-registry-selection.ts:184`). Missing session → resync error. **No empty-snapshot degradation**; old `legacy_snapshot*` stub removed.
- **Config command guardrails**: envelope `baseRevision` read fresh before send (`COMMANDS_REQUIRING_BASE_REVISION` contains switchModelConfig/switchCollaborationMode, `nc-code-core/src/protocol.rs:75`, else `proto.invalidPayload`); command is `Route::Admitted` (spawned task) so `v4/commands/query` returns the settled state; rejection/failure throws truthfully.
- `listSessions` prioritizes runtime id + disk meta to replenish cold sessions.
- Tests: `session.rs` 6 (strict keyset, true model facts, CAS+settle, list/message loop, missing provider fail-closed, unwired fail-fast); crate `18/0`; clippy `-D warnings` and fmt green.

**Unwired 4 members (`F`: fail-closed returns `Method not found`, does not fake empty results)**

| Members | What’s missing / Basis |
| --- | --- |
| ~~`readWorkspacePresentation`~~ | **Wired 2026-11-08**: the protocol directory (builtins `goal`/`compact`/`init`, composer-only `plan`, plus custom commands filtered by reserved names and `disable-noninteractive`, `workflow` pinned after `goal`) is projected by the `commands` channel and injected into `zcode-session` through `SlashCommandDirectory` (`nc-code-app` composition root). |
| `readSessionEvents` | host has no event logging subsystem (source of TS `sessionEvents`) |
| `promoteDeferredDraftSession` | The deferred draft registry is the TS in-process state (`zcodeSessionService.ts` deferredDraftSessions) |
| `closeDeferredDraftSession` | Same as above |

These 4 items are also in `MEMBER_BOUNDARY` of `native/tools/check-channel-coverage.mjs` (quote this section), making "Wiring 11 + Record 4" machine-verifiable.

### 4.5 `zcode-agent` F evidence for unwired members (2026-10-28 member-by-member audit)

> §2 requires 471 members "have implementation or evidence of F". This section records **not included in this product** members with file:line evidence, judged by "no consumer in the Rust product build (`packages/web` + `packages/ui` + Tauri shell)" or "no event source in this host", **not** "migration trouble". Anti-false rules still apply: these members must fail explicitly with a named reason, never count as progress or return empty success.

| Members | Evidence (Consumer/Event Source) | Determination |
| --- | --- | --- |
| `onDynamicProcessResourceSample`, `onDynamicToolExecResource`, `onDynamicMcpResourceSamples`, `onDynamicMcpTelemetry`, `collectLocalRuntimeChildProcesses` | Electron Desktop Resource Window/ARMS only Relay consumption: `packages/desktop/src/host/hostAgentResourceTelemetry.ts:23`, `hostToolExecResourceTelemetry.ts:10`, `hostMcpResourceTelemetry.ts:11`, `hostMcpTelemetry.ts:13`, `hostResourceUsage.ts:52` | `F`: R4「Replace with OTLP" bucket acceptance, in-process host has no child process generation to sample |
| `onDynamicSessionRuntimePreferencesRequest`, `respondSessionRuntimePreferences` | Only `packages/desktop/src/host/remoteWorkspaceServiceCollection.ts:211,288` (remote connection family) | `F`: Remote connection family is not in the scope of this product (§0/non-target) || `subscribeWorkspaceConfigV4`, `resyncWorkspaceConfigV4`, `unsubscribeWorkspaceConfigV4`, `onDynamicWorkspaceConfigFrame` | web build consumerless; backend only `packages/services/src/zcode-agent/zcodeTaskIndexSyncer.ts:1509` | `F`: workspace-config subscribed by task Index synchronization is undertaken, and the Rust side is implemented in `nc-code-services-agent::task` |
| `prepareStorage`, `getStorageStartupState`, `onDynamicStorageStartupState` | There is no consumer in the front end (searching `packages/{ui,web,client,desktop/renderer}/src` has no hits); the storage gate is the TS CLI self-entry | `F`: The in-process host has no "subprocess queued before starting" status to report |
| `onAgentRuntimeRestarted` | The consumer is the directory refresh after runtime recycling (`packages/ui/src/hooks/usePluginReferenceCatalog.ts:108`, `useSkills.ts:63`, transport `:582`), but the event source is the process generation of `zcodeAgentProcessManager` - within this host process, no generation recycling | `F`: Passive event; the skill/plugins directory is instead driven by real file changes in the `hooks`/`plugins` channel |
| `onDynamicSessionEvent` | `packages/services/src/zcode-agent/zcodeAgent.ts:720-725` marked `@deprecated`, leaving only task adapter reference | `F`: replaced by v4 conversation frame |
| `listMcpServerStatuses` | No consumer found in web/desktop; the pool of `native/crates/nc-code-host/src/mcp.rs:282` has no status enumeration | `F`: no consumer; if the MCP panel requires status, first fill in the product definition and then transplant |
| `hasActiveCuaOperationTurn` | `packages/services/src/node.ts:2267` (CUA Helper restored) | `F`: CUA on this host fail-closed (§4.4) |
| `readSessionEvents` | host has no event logging subsystem (same as §4.4.1: Source of TS `sessionEvents`) |
| ~~`readWorkspacePresentation`~~ | Wired 2026-11-08 (same projection as §4.4.1; the `zcode-agent` mirror reads the same port). |
| `testModelConnectivity`, `grantWorkspaceHookTrust`, `onDynamicPluginOperationProgress` | Existing real paths for the same behavior: `provider-settings` (`nc-code-services-provider/src/settings.rs:706`, `catalog.rs:599`), `hooks` (`packages/ui/src/settings/useWorkspaceHookInlineTrust.ts:47,102` + `nc-code-host/src/workspace_hooks/admission.rs:30`), `plugin-management` | `F`: **alias**, disallow second write path (AGENTS.md single owner rule) |

**2026-10-29 Closure: All the "still to be transplanted" items listed above have been implemented**, `pnpm native:coverage` is now reported
323/323 members have evidence of distribution (148 boundaries all with spec provenance). Destinations one by one:- 9 CRUD in automation + `runAutomationNow` with 20s scheduling loop →
  `native/crates/nc-code-services-automation` + `nc-code-services-agent/src/automation_dispatch.rs`
  (claim→dispatch→settle, misfire window jumping, retreat and ledger are all tested on real SQLite).
- saved workflow file layer 5 member → `nc-code-services-agent/src/saved_workflows.rs`;
  `listSavedWorkflowRuns` still remains on the TS engine side as per `specs/workflow-capability-boundary.md`.
- `readSessionMessages` → Mirror distribution point for `build_snapshot` shared with zcode-session.
- `readSessionDebug` / `listSessionSubagents` / `compactSession` / `goalSession` / `generateWorkspaceText` → `native/crates/nc-code-host/src/protocol_methods.rs` (host-side implementation; channel only encodes/decodes + completes snapshot); test `native/crates/nc-code-host/tests/protocol_methods_e2e.rs`.
- `onDynamicSessionsIndexFrame` dispatched by topic → frame bus filters by notification method then delivers params (`nc-code-services-agent::notification_params`).
- `onDynamicLocalTtftFacts` → `nc-code-host/src/session_ttft.rs` checkpoints, filtered by `v4/telemetry/local-ttft`; test `local_ttft_e2e.rs`.
- `disposeWorkspace` / `getWorkspaceRuntimeIdentity` → already wired in §4.4.1 / mirror.
- The subagent running interface (Task tool + subsession + running registry) had no implementation before, `listSessionSubagents` could only return empty — 2026-10-29 implemented together (`nc-code-host/src/subagent.rs`; see `specs/disabled-features.md` §3.6).

**Deep subsystems still not in this product** (not the channel gap; "what the port gives users" is in `specs/disabled-features.md` §4): dynamic workflow engine (stays in TS, §3.3), cross-session durable usage (`v4/usage/stats`), controller facade alias, CUA, and unported parts of the TS built-in tool surface (TS 40 tools, 18 ported, gap 7, boundary 15; gate `node native/tools/check-tool-parity.mjs` + `native/tools/tool-parity.table.json`; see `native/crates/nc-code-host/README.md` Difference 12).
Since 2026-10-29 **plan tool / `v4/conversation/plans`** and **backend bash / `v4/conversation/backgroundBashOutput`** are wired (ENTRY 114/119); the latter still owes three items (timeout auto-background, `progress`/`outputPreview`, `TaskStop`/`TaskOutput` stop surfaces) in `specs/rust-native-crates.md` S3 Rule 8.


## 5. Anti-counterfeiting rules (hard)

1. **Unimplemented channels must not be registered**. Prefer `Unknown channel` over registering a "return `{}`/`[]`" shell; not implemented = visible front-end failure.
2. **No `todo!()`/`unimplemented!()`/placeholder branches**. No code for unstarted channels; each method of a started channel has a real path.
3. **Events must have a true source**. No timer-driven false frames; dynamic events are driven by real host/file/process events.
4. **CUA(`cua-permission`/`cua-pip-session`)**: TS `@zcode/zcode-cua` is "API-compatible placeholder", all runtime faces fail closed. Rust side mirrors the same `available:false`/denial semantics pinned by fixtures — an alignment product fact, not a stub.
5. **Evidence**: (a) wire: `nc-code-rpc` golden (real TS client/server generation); (b) service: per-channel param-decode test + real file/process/status e2e + channel-level request-response (real `nc-code-channels` runtime); (c) front end: real Tauri window boot log without unknown channel/method; (d) bench: `nc-code-rpc` wire bench + key-path bench/latency assertion.
6. **Delete-TS preconditions**: four conditions (`rust-native-crates.md`) plus §0 done definition; never delete TS first and "make it up later".

## 6. Milestones

| # | Content | Acceptance |
| - | --- | --- |
| M1 | `nc-code-rpc` wire (serialization/framing/channel) | ✅ golden 34 vectors (16+4+14) + 20 tests + bench |
| M2 | `nc-code-channels` runtime + `nc-code-server` `/ws` + `/api/server-info` + static | ✅ Real TCP/WS e2e; fragmentation/sticky package, call/listen/cancel, capability, lite-token are all tested |
| M3 | Tauri shell loads the current front-end and connects to the Rust server | ✅ Window process + embedded server; currently boots to the onboarding home screen, the missing channel is exposed as `Unknown channel` (visual screenshots are blocked by the front window) |
| M4 | Configuration/workspace boot collection (setting, broadcast, client-config, client-scenes, onboarding, file, file-watcher, system, git, terminal, zcode-task list, zcode-session list) | UI can open workspace, browse files, read settings, switch themes |
| M5 | agent family wiring `nc-code-host` (session creation/sending messages/streaming frames/stop/recovery/goal/hooks) | real model request; restart recovery; frame semantics consistent with TS backend |
| M6 | Ecological family + remaining configuration surface | 40/40 channel registration, 471/471 members have implementation or F evidence || M7 | Delete the TS backend path, tighten the ratchet, and Tauri uses Rust by default | `pnpm typecheck`/`architecture:check` green; Tauri package does not contain Node |

**2026-10-31 Delete gate audit (Conclusion: Do not delete)**: The default flow cutting function of agent/workflow is ready
(ENTRY 160–163: named profile, direct launch/set revision, two controlOnly wheels, kept-script,
Bash automatic background, `outputPreview`; tool gate and host e2e are all green), but delete the two hard conditions of the TS backend
Still not true:

1. **Desktop provider handover (api-key passed, account type missing)**: host no longer requires `--provider-config`; it reads the two desktop-injected storage files (`ZCODE_BUILTIN_PROVIDER_CONFIG_FILE` / `ZCODE_PERSONAL_PROVIDER_CONFIG_FILE`, `packages/provider-node/src/runtime-paths.ts`) and projects static `ProviderConfig` in `native/crates/nc-code-host/src/provider_handoff.rs`. Evidence: `provider_handoff_e2e` (real host without `--provider-config` + desktop-isomorphic stores, one real model round). **2026-11-04**: desktop-resolved `zhipu-coding-plan-api-key` accepted (`projects_a_desktop_resolved_coding_plan_api_key`). **2026-11-05**: all ZCode account providers removed; only direct API key/personal/custom providers supported. Custom API headers may still need handling; verify with api-key providers.
2. **Desktop e2e not verified**: Electron desktop has never run a real model round against the native host; host e2e covers only post-spawn links (storage parsing, model request, streaming).
3. Tauri/Node removal (S4/S5/S6) of M3–M6 incomplete.

Evidence: ENTRY 157–163, `specs/workflow-capability-boundary.md`, §0/§5. Per `feedback-removal-gate`, TS backend stays online until (1)(2) have real-device test evidence.

**Resume plan (provider handover → desktop e2e → flow cut)**:

1. Resolve provider selection in desktop composition root (`packages/services/src/model-provider/` + credential service), generate static `ProviderConfig` (`kind/baseUrl/apiKey/model/maxTokens`). **Credentials never via argv**: use stdin/pipe or a 0600 temp file (or existing `--provider-config` path deleted after host reads it). Key never logged.
2. Desktop e2e: `ZCODE_NATIVE_HOST=1` real Electron, session + model + tool call; assert streaming frames/tool lines/restart recovery come from native host; provider-parse failure is a named error (not silent empty).
3. Full gate (host lib, workflow/agent e2e, tool gates, typecheck/lint, clippy/fmt) + green desktop e2e → make native host default, delete TS backend (M7); S4/S5/S6 follow.

## 7. Verify command

```sh
# wire + runtime
cargo test -p nc-code-rpccargo test -p nc-code-channels
cargo clippy -p nc-code-rpc -p nc-code-channels --all-targets -- -D warnings

# server (real HTTP/WS)
cargo test -p nc-code-server
cargo run -p nc-code-server -- --workspace <path> --static-root <web-dist> --port 0

# Service surface list drift
node native/tools/inventory-service-channels.mjs

# Completeness access control (channel/member + v4 method boundary; --self-test uses positive and negative examples to prove that access control will fail)
pnpm native:coverage
node native/tools/check-channel-coverage.mjs --self-test

# Front end (build product)
pnpm --filter @zcode/web build
# Tauri (requires GUI environment)
cargo run -p nc-code-shell
```

## 8. Documented differences

1. Unknown channel errors immediately (TS: 1s timeout); fail-fast exposes missing ports, payload `name` stays "Unknown channel".
2. `RpcErrorPayload.stack` has no JS stack; TS client only overwrites `error.stack` when present. Behaviorally equivalent.
3. JSON key order: Rust `serde_json` uses `preserve_order` (TS insertion order); struct-field order must match the TS object literal order. Wire layer is golden-pinned; service layer adds fixtures as needed.
4. Service parameter is a positional array (see §1), not a single object; decode `arg` as an array, never as an object.
5. First Tauri uses the web platform shim (`createWebPlatform`); native Dialog/Updates/Deep Links migrate under R4 independently and do not affect backend integrity.
6. Dynamic events `onDynamicConversationFrame` / `onDynamicSessionsIndexFrame` / `onDynamicLocalTtftFacts` deliver notification **params** (wire frame/fact), not the row JSON-RPC envelope (TS `zcodeAgentService` also strips `message.params` before the fire emitter; earlier Rust forwarded the whole line so `topicWireDecoder`/`LocalTtftObserver` could not read `topic`/`facts`). Fixed 2026-10-29.
7. stdio host is one task per protocol line (`native/crates/nc-code-host/src/main.rs`), not "process then read next"; per-session ordering stays via `Session` FIFO and in-task write order, so `workspace/cancelGenerateText`/`stop` are not queued behind minute-level model calls.
8. **Subscription renewal/ownership** (2026-10-29): `subscribe`/`resync` now use TS `base { logEpoch, seq }` (`specs/rust-native-crates.md` S3 "conversation resume"); ack `mode` reflects `resume` vs `snapshot`. Also fixed: resync previously returned an unrecognized subscription ("ack, zero frame") while `conversationProjectionStore.awaitingInitial` waited for the initial frame — now throws `fault.subscription.notOwned` like TS `v4-gateway.ts` (token in body, client heals). Unported: two-phase commit/rollback of frame reservation and wire-snapshot byte ceiling raising `floorSeq` (see `specs/disabled-features.md` §4.5, `native/crates/nc-code-host/README.md`).
9. **Plan mode** (2026-10-29): TS `planEnabled` orthogonal to `mode`;
 `session-mode-port.ts` only toggles that boolean, `getPrePlanMode()` always undefined. Rust uses `collaboration_mode == "plan"` (goal loop/guard follow it), saving `meta.plan_previous_mode` on entry and restoring on exit. Both project `config.mode` + `config.planEnabled` (`payload.planEnabled ?? mode === "plan"`) + `config.planTransition` (only when tool-driven; integer keys omitted, never `null`, because `sessionConfigStateSchema` is `.optional()`). Contract in `specs/rust-native-crates.md` S3 "plan mode"; remaining diffs (`allowedInPlanMode` undeclared, plan-file write failure does not revoke approval) in README 12a.
10. **Remote workspace routing in the Tauri shell** (2026-11-09): the Tauri platform adapter
 `packages/web/src/tauriRemote.ts` calls the embedded Rust server's `/api/remote/*` routes
 directly (`connect` / `bind` / `dispose`, `cancelPendingRemoteConnection` via an
 `AbortController` scoped by requestId). The server's registry is the single owner of the
 remote session id; a 2xx without `remoteSessionId` is reported as a failure, and the
 server's error text is surfaced verbatim. `target.port` is the SSH transport port
 (`ssh -p`, pinned by `remote_registry::tests::ssh_tunnel_args_pin_the_transport_shape`);
 `target.remotePort` (the remote `zcode-server` port) is still required — discovering it is
 part of the **unported remote deploy** family (`packages/server/src/remote/*`,
 `packages/zcode-server-cli`), so an SSH connect without it fails with that named reason.
 `wsl`/`docker` kinds remain host-side bind transports (`POST /api/remote/bind`), exactly as
 the server documents. Tests: `packages/web/test/tauriRemote.test.ts` (8) and
 `nc-code-server/tests/remote_api.rs` (5, incl. invalid `target.port`).