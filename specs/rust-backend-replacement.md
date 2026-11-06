# Spec: Rust backend replacement (TS service channels → Rust channel server)

> **WARNING**: Never trust docs/logs without fully auditing code. Always verify against actual implementation.
Status: accepted 2026-10-27. M1–M3 landed; M4–M7 in execution.
Z.ai/ZCode boundary: `specs/zai-claim-boundary.md` (updated 2026-11-05 - all ZCode services removed; every provider still works by default). **Always audit code, not just docs.**
Date: 2026-10-27
Owner: native backend
Governing specs: `specs/rust-native-crates.md` (crate map / dependency direction; this spec adds
to it), `specs/typescript-frontend-only.md` (the language boundary ratchet),
`specs/rust-pure-native-roadmap.md` (phase definitions). On conflict: crates and dependency
directions follow this spec + the `rust-native-crates.md` additions; the language boundary follows
`typescript-frontend-only.md`.

## 0. Goal and definition of done

**Goal**: the current frontend (`packages/web`, and the future Tauri window) changes no calling
convention, while the backend (HTTP bootstrap, WebSocket channel RPC, all 40 service channels) is
implemented in Rust. The Tauri application loads the **current frontend build** and connects to
the **in-process Rust backend**.

**Definition of done** (every item is required):

1. Every channel in `native/fixtures/service-channels.json` is registered in the Rust registry,
   and every `role = "rpc"` member has a real implementation (no `todo!()`, no always-true /
   always-empty stub, no "hook it up later" branch).
2. Every `role = "event"` / `"dynamic-event"` member has a real event source: events come from
   real state changes (file watching, session frames, process sampling), not from a timer
   faking them.
3. Frontend boot and the core flows (open workspace → session list → send message → tool call →
   streaming frames → stop → restart recovery) run through inside a **real Tauri window**, with no
   unknown channel/method in the backend logs.
4. For capabilities that cannot exist in this product (currently: the CUA placeholder package and
   the "remote connection" family), the Rust side returns the same fail-closed result as TS and a
   test pins it — this is a product fact, not a stub.
5. The TS backend entry points (`packages/server`'s HTTP/WS startup path, `createLocalServices`'
   runtime assembly) are deleted or moved into `old.back/` after the frontend switches to Rust
   completely, and the `typescript-frontend-only` ratchet tightens in step.

**Machine definition for done items 1–2**: `pnpm native:coverage`
(`native/tools/check-channel-coverage.mjs`; a non-zero exit means not done). It places every
member name of `service-channels.json` into its channel using the **dispatch evidence** on the
Rust side — first resolving that channel's dispatch range through
`registry.register("x", self.field)` → field type → `impl Channel for Type`, then searching inside
the range for `"name" =>` / `"name" |` / `== != "name"` / `.insert("name"`; so same-named members
(`list`, `get`, `create`) cannot borrow another channel's dispatch to pass. A member that was
explicitly removed only counts when that member / that channel finds a **spec-cited reason** in
the tool's `CHANNEL_BOUNDARY` / `MEMBER_BOUNDARY`, otherwise it errors — "removal" is a product
decision and must not be used to cover a missed port; conversely, an entry written in the boundary
list that does not exist in the TS member table is an error too. A registered channel for which the
tool sees no evidence at all inside its dispatch range is also an error; silently counting it as 0
is not allowed.

**Non-goals**: rewriting the frontend UI; Electron→Tauri platform capabilities (dialogs / updates
/ deep links / BrowserView) advance independently under the R4 bucket of
`rust-native-crates.md`. This spec only requires the minimal shell surface needed to run.

## 1. Current-state evidence (measured 2026-10-27)

| Area | Files | Lines |
| -------------------------- | -----: | --------: |
| `packages/rpc/src`         |     16 |     3,295 |
| `packages/services/src`    |    298 |    87,469 |
| `packages/server/src`      |     48 |    10,771 |
| `packages/client/src`      |      6 |       723 |
| `packages/desktop/src/main`|    188 |    47,450 |
| `packages/web/src` (frontend) |     16 |     2,875 |

- Service-surface inventory (machine generated, manual edits forbidden):
  `node native/tools/inventory-service-channels.mjs` →
  `native/fixtures/service-channels.json`.
  Current: **40 channels / 437 rpc / 6 events / 28 dynamic events** (471 members in total).
- Web startup contract (`packages/web/src/main.tsx`):
  1. `GET /api/server-info` → `ServerRemoteInfo` (`workspaces[0].path/workspaceIdentity` optional);
  2. `new WebSocket(ws[s]://<origin>/ws)`, binary frames;
  3. `SocketProtocol` → `ChannelClient` → `RemoteServiceAccess` (`ProxyChannel.toService`).
- Channel wire (already ported byte-for-byte in `native/crates/zcode-rpc` and passing golden):
  13-byte frame header + `[type,id,channel,method] + arg`, `RequestType` 100–103,
  `ResponseType` 200–204. The golden is produced by driving the real `ChannelClient`/`ChannelServer`.
- **Argument shape**: `ProxyChannel.toService`'s `call` always sends a **positional argument
  array** (`channel.call(propKey, [...args])`). The server-side handler receives a
  `WireValue::Array` and decodes it in order; an event listener's argument is `undefined` (plain
  event) or the call arguments (`onDynamicXxx(arg)`).
- Event naming convention (`proxy-channel.ts`): `on[A-Z]…` = plain event (`listen(name)`);
  `onDynamic[A-Z]…` = dynamic event (`listen(name, arg)`); everything else is an RPC method.
  An unknown server-side method throws `Method not found: <name>`; an unknown event throws
  `Event not found: <name>`.

## 2. Target architecture

```
┌────────────────────────────────── Tauri window (WebView2/WKWebView) ───────────────────────────────────┐
│ packages/web build output (unchanged)                                                                  │
│    GET /api/server-info      WS /ws (binary frames)                                                    │
└───────────────▲──────────────────────────▲─────────────────────────────────────────────────────────────┘
                │ HTTP                     │ channel RPC
┌───────────────┴──────────────────────────┴─────────────────────────────────────────────────────────────┐
│ zcode-shell (Tauri 2): setup starts the embedded server (127.0.0.1:0), window navigates to that origin │
└───────────────▲────────────────────────────────────────────────────────────────────────────────────────┘
                │
┌───────────────┴────────────────────────────────────────────────────────────────────────────────────────┐
│ zcode-server (axum): /healthz, /api/server-info, /ws upgrade, static assets + SPA fallback             │
└───────────────▲────────────────────────────────────────────────────────────────────────────────────────┘
                │ Registry<Channel>
┌───────────────┴────────────────────────────────────────────────────────────────────────────────────────┐
│ zcode-app (composition root): data dir, settings, workspace, registry wiring, lifecycle                │
│   ├─ zcode-services-config     setting/broadcast/client-config/…                                       │
│   ├─ zcode-services-workspace  file/git/terminal/watcher/system/…                                      │
│   ├─ zcode-services-agent      zcode-agent/zcode-session/zcode-task/window-controller                  │
│   └─ zcode-services-ecosystem  skills/plugins/mcp/subagents/commands/hooks/memory/…                    │
└───────────────▲────────────────────────────────────────────────────────────────────────────────────────┘
                │ in-process
┌───────────────┴────────────────────────────────────────────────────────────────────────────────────────┐
│ zcode-host (already exists): v4 conversation runtime, tool loop, goal, workspace hooks, MCP            │
│ zcode-core/provider/watch/proc/image/diff/scan: existing native capabilities                           │
└────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

### 2.1 New crates and dependency direction

| crate | Responsibility | Allowed dependencies |
| ------------------------- | -------------------------------------------------------------------- | -------- |
| `zcode-rpc` (already exists) | channel wire encode/decode, frames, golden contract | serde/serde_json |
| `zcode-channels` (new) | connection runtime: registry, Initialize, call/listen/cancel, EventFire forwarding | `zcode-rpc`, tokio |
| `zcode-service-core` (new) | shared service-layer pieces: positional argument decoding, typed JSON, ServiceError → RpcErrorPayload, path/atomic write, event bus | `zcode-rpc`, serde, tokio |
| `zcode-services-config` (new) | config/credential/account/usage family channels | `zcode-service-core`, `zcode-channels` |
| `zcode-services-workspace` (new) | file/git/terminal/watcher/system family channels | same as above + `zcode-watch`, `zcode-proc`, `zcode-scan`, `zcode-image`, `zcode-diff` |
| `zcode-services-agent` (new) | agent/session/task/window-controller | same as above + `zcode-host`, `zcode-provider` |
| `zcode-services-ecosystem` (new) | skills/plugins/mcp/subagents/commands/hooks/memory/share/feedback/sync | same as above |
| `zcode-app` (new) | composition root: paths, config loading, registry assembly, disposer | all services crates |
| `zcode-server` (already exists) | axum HTTP/WS/static; `serve(app, options)` is embeddable | `zcode-app`, `zcode-channels`, `zcode-rpc` |
| `zcode-shell` (already exists) | Tauri: start the server, navigate the window | `zcode-app`, `zcode-server` |

**Forbidden**: a service crate depending on `zcode-server`/`zcode-shell`; `zcode-channels`
depending on any service crate; circular dependencies. A new edge must first amend this table,
then be written in code.

### 2.2 Ownership and event order

- **Each connection** owns its channel context (clientMode, connectionId); service instances are
  process-level singletons and so is the event bus; subscriptions are connection-level.
- **Event order**: events of the same channel must keep their commit order (a broadcast has one
  ordered sender). A connection writes events into a single FIFO outbound queue, and
  **responses and events share that order**; there is no "send the response first, then an earlier
  event".
- **Cancellation**: `PromiseCancel` only cancels the token of that request; `EventDispose` only
  unsubscribes the corresponding id. Long tasks (agent stream, terminal) must stop writing events
  downstream once the token is cancelled.
- **Idempotence**: repeated `EventDispose`/`PromiseCancel` are no-ops; a response after
  cancellation is discarded.

## 3. Channel runtime contract (`zcode-channels`)

- Connection established → send `Initialize` immediately (`ResponseType.Initialize`), matching the
  TS `ChannelServer`, which is ready at construction time.
- Frame intake: `FrameDecoder` tolerates fragmentation/sticky packets; only `Regular` frames are
  accepted as channel messages; non-Regular frames are ignored by type (PersistentProtocol
  semantics are added by a later milestone).
- Requests:
  - `Promise`: decode the header (the 4-tuple) and the body; `arg` is handed to the channel as-is
    (the service layer decodes it as an array).
  - `EventListen`: plain events ignore the arg; dynamic events pass the arg to the channel.
  - Unknown channel: return `PromiseError{name:"Unknown channel"}` **immediately** (TS times out
    after 1s; the recorded difference for this host is: fail-fast, so a "missed-ported channel" is
    not hidden behind a 1s delay). An `EventListen` on an unknown channel is dropped outright (TS
    drops it after the timeout too).
  - Unknown method: `PromiseError{name:"Error", message:"Method not found: <name>"}`.
  - A handler returning `Err(ServiceError)` → `PromiseError`; a returned `Err(Thrown)` that is not
    an Error → `PromiseErrorObj` (aligned with TS's non-Error rejection branch).
- Events: a channel subscription produces a `WireValue` stream; the connection sends
  `EventFire{id}` for each value. On `EventDispose`/connection close the subscription task is
  dropped and must not leak.
- Frame ceiling 64 MiB (`zcode-rpc` default); exceeding it disconnects the connection and records a
  diagnostic.
- Argument/return-value codec uses `WireValue` only: top-level bytes go through Buffer/VSBuffer,
  nested bytes follow the `__zcode_rpc_nested_uint8array_v1` marker
  (`serialization::nested_uint8array_*`).

## 4. Service migration matrix (grouped by channel)

> Status legend: `—` not started; `W` wired and tested; `F` product unavailable, fail-closed alignment (with evidence);
> `R` is removed from the Rust backend as per `specs/zai-claim-boundary.md` (not registered, not ported). **The common model capabilities of any provider are not affected. **
> Every time a channel is completed, update this table and attach the test location; **The table is the release threshold, not a plan**.

### 4.1 Configuration/Account Family (`zcode-services-config`, 13 channels / ~110 rpc)

| channel | rpc | events | status | dependencies/notes |
| --- | ---: | ---: | --- | --- |
| setting | 4 | 0 | W | `zcode-services-config::setting`; schema/defaults generated by real Zod; test 17 |
| broadcast | 5 | 1 | W | `zcode-services-config::broadcast`; claim state machine + event; test 3 |
| client-config | 1 | 0 | W/D | `zcode-services-client`; **Remote reading is disabled** (`specs/disabled-features.md` §2.3), local returns `{pluginStoreOrder:null}` |
| client-scenes | 1 | 0 | W/D | `zcode-services-client`; **Remote reading is disabled** (`specs/disabled-features.md` §2.4), local returns empty directory envelope |
| onboarding-record | 8 | 0 | W | `zcode-services-config::onboarding`; file reading/writing/claiming/backfilling; test 3 |
| settings-sync | 6 | 0 | R | **Remove** (upload settings) |
| provider-settings | 15 | 1 | W | `zcode-services-provider`; personal provider/model store, templates, CRUD, resolve, real connectivity detection; 8 tests |
| model-selection | 2 | 1 | W | `zcode-services-provider`; projected candidate by personal store; empty view consistent with TS boot fixture field by field |
| provider-provisioning-target | 1 | 0 | F/W | Trusted host only; writable by local machine, rejected by web client |
| credential | 3 | 0 | W | `zcode-services-config::credential`; AES-256-GCM (TS ciphertext fixture decryptable); test 5 |
| zai-claim (new) | 3 | 0 | R | **Removed** (all ZCode services removed per `specs/zai-claim-boundary.md` updated 2026-11-05) |
| oauth | 13 | 0 | R | **Removed** (all ZCode services removed per `specs/zai-claim-boundary.md` updated 2026-11-05) |
| usage-stats | 8 | 0 | R | **Remove** (remote usage/limit query, not claim data) |
| coding-plan-subscription | 4 + 25 removed | 0 | R | **Removed** (all ZCode services removed per `specs/zai-claim-boundary.md` updated 2026-11-05) |

### 4.2 Workspace family (`zcode-services-workspace`, 8 channels / ~68 rpc)

| channel | rpc | events | status | dependencies/notes |
| --- | ---: | ---: | --- | --- |
| file | 17 | 0 | W | `zcode-services-workspace::file`; read-write/scope/binary/preview, workspace creation, index chunking, `.zcodeignore` read-write/sync/reset; 11 tests |
| media-preview | 3 | 0 | R | **Removed** (overridden by file.readMediaPreview; standalone channels are no longer registered) |
| file-watcher | 4 | 1 | W | `zcode-services-workspace::watcher`; notify 8, 150ms anti-shake, single-path transparent transmission, unwatch/disposeAll; 12 tests |
| git | 18 | 0 | W | `zcode-services-git::channel` (all 18 are transplanted according to the scope of `gitService.ts`); subprocess git, args array disabled shell; test 45 |
| git-checkpoint | 4 | 0 | W | `zcode-services-git-checkpoint` (repo/store/service/helpers layer corresponds to TS one-to-one): temporary `GIT_INDEX_FILE` → write-tree/commit-tree → hidden ref; restored after three-party conflict verification (missing/unexpected/type/content); test 2 real git warehouses e2e (round-trip+conflict+force, guard and damage manifest fail-closed) |
| terminal | 6 | 0 | W | `zcode-services-terminal` (portable-pty 0.9 ConPTY); single test 46 + channel e2e 6 + pty e2e 5 all green (ENTRY 103: ConPTY sends `ESC[6n` when starting, the test end must return the cursor position report; at the same time, the pump thread blocking when the child process exits causes `onDynamicExit` deadlock) |
| system | 3 | 0 | W | `zcode-services-workspace`; info/terminal shell probe/intranet TCP+HTTP probe; 4 tests |
| window-controller | 8 | 0 | W | `zcode-services-client::window_controller`; only routed to `zcode-task` facade (single source of truth), controller frames are pushed by topic |

### 4.3 Agent/session family (`zcode-services-agent`, 3 channels / ~196 rpc + 16 events)

| channel | rpc | events | status | dependencies/notes |
| --- | ---: | ---: | --- | --- || zcode-agent | 100 | 16 | W | 100 rpc All have distribution points (`pnpm native:coverage` counts). Core: in-process `zcode-host` (initialize, create/send/read/close/list, v4 subscribe/resync/unsubscribe, v4 command/query, rowsRange, fileChanges, rewindPreview, hello/init/flow, runtime prefs/identity, frame/lifecycle events); 5 mirror members shared with zcode-session `build_snapshot`/`run_config_command` single write path; plugin 18 members mirrored to `plugin-management`; automation 10 members + saved workflow 5 members; 2026-10-29 Complete session reading surface/control surface `readSessionDebug`, `listSessionSubagents`, `compactSession`, `goalSession`, `generateWorkspaceText` and event `onDynamicLocalTtftFacts` (see `native/crates/zcode-host/src/protocol_methods.rs`, `session_debug.rs`, `session_ttft.rs`, `subagent.rs` for implementation). Only `onDynamicSessionEvent` is not wired according to the F evidence in §4.5 (TS has been abandoned, and this host has no second event source). |
| zcode-session | 15 | 0 | — | Session application service |
| zcode-task | 65 | 0 | W | `zcode-services-agent::task`; 59 rpc + 6 events all have distribution points (`pnpm native:coverage` is counted), method-level tests are still completed according to §6 |

### 4.4 Ecosystem (`zcode-services-ecosystem`, 16 channels / ~106 rpc + 0 events)

| channel | rpc | status | remarks |
| --- | ---: | --- | --- |
| commands | 6 | W | `zcode-services-extensions::commands`, test including write/disable/rename/delete full loop |
| memory | 2 | W | `zcode-services-extensions::memory`, real reading and writing of the project memory directory |
| skills | 6 | W | `zcode-services-extensions::skills` (compiled for the first time after completing the `mod skills` statement in this round) |
| hooks | 3 | W | `zcode-services-extensions::hooks`; reuse host trust store/admission, no second writing path |
| subagents | 8 | W | `zcode-services-extensions::subagents`; session subagent registration interface |
| plugins | 7 | W | `zcode-services-plugins::service`; The TS product itself has retired this channel (empty overview + `supported:false`, changing members throws an error), Rust mirroring one by one = parity |
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

- **The only construction point of a snapshot** is `AgentService::build_snapshot` → `snapshot::build` (strictly
  `zcodeSessionStateSnapshotSchema`), input = host session fact(view/transcription/usage) +
  v4 authoritative snapshot + provider store’s model facts (`settings_facts` aligned field by field
  `toModelOption`, `provider-registry-selection.ts:184`). When the session does not exist, resync will directly report an error.
  **No longer degrades into empty snapshots**; the old `legacy_snapshot*` stub (fake status/turnCount/messages) has been removed.
- **Configuration command two guardrails**: Envelope with `baseRevision` read freshly before sending (`COMMANDS_REQUIRING_BASE_REVISION`
  Contains switchModelConfig/switchCollaborationMode, `zcode-core/src/protocol.rs:75`, otherwise
  `proto.invalidPayload`); the command is `Route::Admitted` (executed in spawned task), use before returning
  `v4/commands/query` It is the turn to settle the final state, rejection/failure will be thrown up truthfully - otherwise the snapshot will read the pre-execution state.
- `listSessions` prioritizes runtime id and disk meta to replenish cold sessions (meta may not be immediately dropped to disk after createSession).
- Tests: `session.rs` 6 (strict keyset, true model facts, CAS+settle, list/message loop,provider is absent fail-closed, unwired members fail-fast); crate `18/0`; clippy `-D warnings` and
  fmt are all green.

**Unwired 4 members (`F`: fail-closed returns `Method not found`, does not fake empty results)**

| Members | What’s missing / Basis |
| --- | --- |
| `readWorkspacePresentation` | Requires the **built-in slash command set** of `listProtocolSlashCommands` (`workspace-model-runtime.ts:27`); returning only the file side command will forge "no built-in command" |
| `readSessionEvents` | host has no event logging subsystem (source of TS `sessionEvents`) |
| `promoteDeferredDraftSession` | The deferred draft registry is the TS in-process state (`zcodeSessionService.ts` deferredDraftSessions) |
| `closeDeferredDraftSession` | Same as above |

These 4 items are also registered into `MEMBER_BOUNDARY` of `native/tools/check-channel-coverage.mjs`
(quote this section), making "Wiring 11 + Record 4" a complete, machine-verifiable account.

### 4.5 `zcode-agent` F evidence for unwired members (2026-10-28 member-by-member audit)

> §2 The definition of completion requires that 471 members "have implementation or evidence of F". This section records **determined to be not included in this product**
> `zcode-agent` member and its evidence; judged based on "In the Rust product build (`packages/web` +
> There is no consumer in `packages/ui` + Tauri shell)" or "the event source does not exist in this host",
> **Not** "migration trouble". The evidence is file:line. During review, recheck by line number; if the product profile changes, this section must be rejudged.
> Anti-false constraints are still in effect: these members are not allowed to be counted as progress, nor are they allowed to be passed with empty returns - calling them must
> Explicitly fail and specify the reason.

| Members | Evidence (Consumer/Event Source) | Determination |
| --- | --- | --- |
| `onDynamicProcessResourceSample`, `onDynamicToolExecResource`, `onDynamicMcpResourceSamples`, `onDynamicMcpTelemetry`, `collectLocalRuntimeChildProcesses` | Electron Desktop Resource Window/ARMS only Relay consumption: `packages/desktop/src/host/hostAgentResourceTelemetry.ts:23`, `hostToolExecResourceTelemetry.ts:10`, `hostMcpResourceTelemetry.ts:11`, `hostMcpTelemetry.ts:13`, `hostResourceUsage.ts:52` | `F`: R4「Replace with OTLP" bucket acceptance, in-process host has no child process generation to sample |
| `onDynamicSessionRuntimePreferencesRequest`, `respondSessionRuntimePreferences` | Only `packages/desktop/src/host/remoteWorkspaceServiceCollection.ts:211,288` (remote connection family) | `F`: Remote connection family is not in the scope of this product (§0/non-target) || `subscribeWorkspaceConfigV4`, `resyncWorkspaceConfigV4`, `unsubscribeWorkspaceConfigV4`, `onDynamicWorkspaceConfigFrame` | web build consumerless; backend only `packages/services/src/zcode-agent/zcodeTaskIndexSyncer.ts:1509` | `F`: workspace-config subscribed by task Index synchronization is undertaken, and the Rust side is implemented in `zcode-services-agent::task` |
| `prepareStorage`, `getStorageStartupState`, `onDynamicStorageStartupState` | There is no consumer in the front end (searching `packages/{ui,web,client,desktop/renderer}/src` has no hits); the storage gate is the TS CLI self-entry | `F`: The in-process host has no "subprocess queued before starting" status to report |
| `onAgentRuntimeRestarted` | The consumer is the directory refresh after runtime recycling (`packages/ui/src/hooks/usePluginReferenceCatalog.ts:108`, `useSkills.ts:63`, transport `:582`), but the event source is the process generation of `zcodeAgentProcessManager` - within this host process, no generation recycling | `F`: Passive event; the skill/plugins directory is instead driven by real file changes in the `hooks`/`plugins` channel |
| `onDynamicSessionEvent` | `packages/services/src/zcode-agent/zcodeAgent.ts:720-725` marked `@deprecated`, leaving only task adapter reference | `F`: replaced by v4 conversation frame |
| `listMcpServerStatuses` | No consumer found in web/desktop; the pool of `native/crates/zcode-host/src/mcp.rs:282` has no status enumeration | `F`: no consumer; if the MCP panel requires status, first fill in the product definition and then transplant |
| `hasActiveCuaOperationTurn` | `packages/services/src/node.ts:2267` (CUA Helper restored) | `F`: CUA on this host fail-closed (§4.4) |
| `readSessionEvents` | host has no event logging subsystem (same as §4.4.1: Source of TS `sessionEvents`) |
| `readWorkspacePresentation` | Requires `listProtocolSlashCommands` built-in slash command set (same as §4.4.1) |
| `testModelConnectivity`, `grantWorkspaceHookTrust`, `onDynamicPluginOperationProgress` | Existing real paths for the same behavior: `provider-settings` (`zcode-services-provider/src/settings.rs:706`, `catalog.rs:599`), `hooks` (`packages/ui/src/settings/useWorkspaceHookInlineTrust.ts:47,102` + `zcode-host/src/workspace_hooks/admission.rs:30`), `plugin-management` | `F`: **alias**, disallow second write path (AGENTS.md single owner rule) |

**2026-10-29 Closure: All the "still to be transplanted" items listed above have been implemented**, `pnpm native:coverage` is now reported
323/323 members have evidence of distribution (148 boundaries all with spec provenance). Destinations one by one:- 9 CRUD in automation + `runAutomationNow` with 20s scheduling loop →
  `native/crates/zcode-services-automation` + `zcode-services-agent/src/automation_dispatch.rs`
  (claim→dispatch→settle, misfire window jumping, retreat and ledger are all tested on real SQLite).
- saved workflow file layer 5 member → `zcode-services-agent/src/saved_workflows.rs`;
  `listSavedWorkflowRuns` still remains on the TS engine side as per `specs/workflow-capability-boundary.md`.
- `readSessionMessages` → Mirror distribution point for `build_snapshot` shared with zcode-session.
- `readSessionDebug` / `listSessionSubagents` / `compactSession` / `goalSession` /
  `generateWorkspaceText` → `native/crates/zcode-host/src/protocol_methods.rs`
  (The only implementation on the host side), the channel only does parameter encoding and decoding + snapshot completion; test
  `native/crates/zcode-host/tests/protocol_methods_e2e.rs`.
- `onDynamicSessionsIndexFrame` is dispatched by topic → the frame bus is filtered by the notification method and then delivered params
  (`zcode-services-agent::notification_params`).
- `onDynamicLocalTtftFacts` → `zcode-host/src/session_ttft.rs` Send checkpoint,
  Channels are filtered by `v4/telemetry/local-ttft`; testing `local_ttft_e2e.rs`.
- `disposeWorkspace` / `getWorkspaceRuntimeIdentity` → Already wired in §4.4.1/Mirror side.
- The subagent running interface (Task tool + subsession + running registration form) has no implementation before, `listSessionSubagents`
  Can only return empty - 2026-10-29 implemented together (`zcode-host/src/subagent.rs`), please see the boundary and reasons
  `specs/disabled-features.md` §3.6.

**Deep subsystems that are still not in this product** (not the channel membership gap, the list with "what will the port give users" is written in
`specs/disabled-features.md` §4): dynamic workflow engine (stay in TS, §3.3), cross-session durable usage
(`v4/usage/stats`), controller facade alias, CUA, and the parts of the TS built-in tool surface that are not included in this host
(Code Facts: TS 40 built-in tools, 18 ported, gap 7, boundary 15; gate
`node native/tools/check-tool-parity.mjs` + `native/tools/tool-parity.table.json`,
For detailed reasons, see `native/crates/zcode-host/README.md` Difference 12).
Starting from 2026-10-29, **plan tool and `v4/conversation/plans`**, **backend bash and
`v4/conversation/backgroundBashOutput`** is no longer listed - they are wired (ENTRY 114 and 119),
Among them, the latter still owes three things (automatic transfer to the background after timeout, `progress`/`outputPreview` on the line,`TaskStop`/`TaskOutput` model side stop surfaces) are itemized in `specs/rust-native-crates.md` S3 Rule 8.


## 5. Anti-counterfeiting rules (hard)

1. **Unimplemented channels are not allowed to be registered**. Would rather return `Unknown channel` than register one"Return all
   `{}`/`[]`" shell; not implemented = The call failed on the front end and the log is visible.
2. **No `todo!()`/`unimplemented!()`/placeholder branches**. No code is written for unstarted channels;
   Started channel Each method has a real path.
3. **The event must have a true source**. It is forbidden to use timers to periodically send false frames; dynamic events must be generated by the real host/file/
   Process event driven.
4. **CUA(`cua-permission`/`cua-pip-session`)**: TS’s `@zcode/zcode-cua` is
   "API-compatible placeholder" (original package description), all runtime faces fail-closed. Rust side implementation
   Same `available:false`/denial semantics, and pinning with fixtures - this is an alignment product fact, not a stub.
5. **Evidence**:
   - wire layer: `zcode-rpc` golden (real TS client/server generation);
   - Service layer: Each channel has at least (a) parameter decoding single test, (b) e2e of real files/processes/status,
     (c) Channel-level request-response testing (real runtime with `zcode-channels`);
   - Front end: Tauri window real startup + boot log without unknown channel/method;
   - Benchmark: `zcode-rpc` wire bench; service layer key path (file index, git status, frame forwarding)
     There must be a bench or reproducible latency assertion.
6. **Delete TS preconditions**: four conditions (see `rust-native-crates.md`) plus the completion definition of §0 of this spec;
   Do not delete TS first and then "make it up later".

## 6. Milestones

| # | Content | Acceptance |
| - | --- | --- |
| M1 | `zcode-rpc` wire (serialization/framing/channel) | ✅ golden 34 vectors (16+4+14) + 20 tests + bench |
| M2 | `zcode-channels` runtime + `zcode-server` `/ws` + `/api/server-info` + static | ✅ Real TCP/WS e2e; fragmentation/sticky package, call/listen/cancel, capability, lite-token are all tested |
| M3 | Tauri shell loads the current front-end and connects to the Rust server | ✅ Window process + embedded server; currently boots to the onboarding home screen, the missing channel is exposed as `Unknown channel` (visual screenshots are blocked by the front window) |
| M4 | Configuration/workspace boot collection (setting, broadcast, client-config, client-scenes, onboarding, file, file-watcher, system, git, terminal, zcode-task list, zcode-session list) | UI can open workspace, browse files, read settings, switch themes |
| M5 | agent family wiring `zcode-host` (session creation/sending messages/streaming frames/stop/recovery/goal/hooks) | real model request; restart recovery; frame semantics consistent with TS backend |
| M6 | Ecological family + remaining configuration surface | 40/40 channel registration, 471/471 members have implementation or F evidence || M7 | Delete the TS backend path, tighten the ratchet, and Tauri uses Rust by default | `pnpm typecheck`/`architecture:check` green; Tauri package does not contain Node |

**2026-10-31 Delete gate audit (Conclusion: Do not delete)**: The default flow cutting function of agent/workflow is ready
(ENTRY 160–163: named profile, direct launch/set revision, two controlOnly wheels, kept-script,
Bash automatic background, `outputPreview`; tool gate and host e2e are all green), but delete the two hard conditions of the TS backend
Still not true:

1. **Desktop provider handover (api-key is passed, account type is still missing)**: host no longer requires `--provider-config` to
   Get the model - it directly reads the two provider storage files that the desktop has injected into each agent spawn
   (`ZCODE_BUILTIN_PROVIDER_CONFIG_FILE` / `ZCODE_PERSONAL_PROVIDER_CONFIG_FILE`,
   `packages/provider-node/src/runtime-paths.ts`), in
   The static `ProviderConfig` is projected in `native/crates/zcode-host/src/provider_handoff.rs`.
   Evidence: `provider_handoff_e2e` uses a real host process without `--provider-config` + desktop isomorphism
   The provider stores a completed round of real models. **Progress (2026-11-04)**: `zhipu-coding-plan-api-key`
   access (the desktop resolves and stores the coding-plan key before spawn) is now accepted and covered by
   `projects_a_desktop_resolved_coding_plan_api_key`. **Updated (2026-11-05)**: All ZCode account providers removed; only direct API key/personal/custom providers supported. Custom API headers may still need handling; verify handover with api-key providers.
2. **Desktop end-to-end not verified**: Electron desktop has never run a real model round with native host; host-level e2e only covers
   Links after spawn (provider storage parsing, model request, streaming reply).
3. Tauri/Node removal (S4/S5/S6) of M3–M6 is still not completed.

Evidence: ENTRY 157–163, `specs/workflow-capability-boundary.md`, §0/§5 of this spec. press
`feedback-removal-gate`, the TS backend remains online until (1)(2) there is evidence of testing on real devices.

**Resume plan (provider handover → desktop e2e → flow cut)**:

1. Resolve the current provider selection in the desktop composition root (there are already `packages/services/src/model-provider/` and
   credential service), generate the static `ProviderConfig` required by the host (`kind/baseUrl/apiKey/model/
   maxTokens`). **Credentials must not be passed via argv** (process list visible): add stdin/pipe to host or
   0600 The temporary file transfer method, or just pass the path to the existing `--provider-config` and delete it after the host reads it.
   Key is never printed in logs/errors.
2. Desktop e2e: `ZCODE_NATIVE_HOST=1` Start real Electron, create a session, send a required model and call a tool
   prompt, asserting that streaming frames, tool lines, and restart recovery all come from the native host; at the same time, verify that the provider parsing fails
   Named error (not a silent empty answer).
3. Full access control (host lib, all workflow/agent e2e, tool three gates, typecheck/lint, clippy/fmt) +
   After the above desktop e2e is green, set the native host as the default and delete the TS backend (M7); S4/S5/S6 will follow.

## 7. Verify command

```sh
# wire + runtime
cargo test -p zcode-rpccargo test -p zcode-channels
cargo clippy -p zcode-rpc -p zcode-channels --all-targets -- -D warnings

# server (real HTTP/WS)
cargo test -p zcode-server
cargo run -p zcode-server -- --workspace <path> --static-root <web-dist> --port 0

# Service surface list drift
node native/tools/inventory-service-channels.mjs

# Completeness access control (channel/member + v4 method boundary; --self-test uses positive and negative examples to prove that access control will fail)
pnpm native:coverage
node native/tools/check-channel-coverage.mjs --self-test

# Front end (build product)
pnpm --filter @zcode/web build
# Tauri (requires GUI environment)
cargo run -p zcode-shell
```

## 8. Documented differences

1. An unknown channel will report an error immediately (TS: 1s timeout). Reason: fail-fast exposes missing port; payload
   `name` remains "Unknown channel".
2. `RpcErrorPayload.stack` does not generate a JS stack; the TS client only overwrites `error.stack` when it has a value.
   Behavioral equivalence.
3. JSON object key order: Rust `serde_json` turns on `preserve_order`, which is consistent with the TS insertion order;
   However, the order of fields from the `serde` structure is determined by the struct declaration and needs to be consistent with the TS object literal order.
   (The wire layer is already golden pinned; the service layer adds fixtures as needed).
4. The service parameter is a positional parameter array (see §1), not a single object; Rust channel must be decoded as an array,
   You cannot treat `arg` directly as an object.
5. The first version of Tauri uses the existing web platform shim (`createWebPlatform`) on the front end, with native capabilities of the platform
   (Dialog/Updates/Deep Links) Press R4 to migrate independently; this does not affect backend integrity determination.
6. Dynamic events `onDynamicConversationFrame` / `onDynamicSessionsIndexFrame` /
   `onDynamicLocalTtftFacts` delivers the **params** (wire frame/fact object) of the notification, not the entire row
   JSON-RPC envelope. The `zcodeAgentService` on the TS side also removes `message.params` in the transport.
   Then use the fire emitter; earlier versions of Rust directly forwarded the entire line, and the renderer
   `topicWireDecoder`/`LocalTtftObserver` cannot read `topic`/`facts`. Corrected on 2026-10-29.
7. The distribution of stdio host is no longer "process one line and then read the next line": one task per protocol line
   (`native/crates/zcode-host/src/main.rs`). Responses and frames for the same session are still unique through `Session`
   FIFO co-order, the "response prior to own notification" for the same request is guaranteed by the writing order within the task; the reason for changing concurrency is
   Control requests such as `workspace/cancelGenerateText` and `stop` cannot be queued after a minute-level model request.
8. **conversation subscription renewal and ownership** (2026-10-29): `subscribe`/`resync` Now press
   TS criterion `base { logEpoch, seq }` (for the contract, see `specs/rust-native-crates.md` S3"conversation resume"), the `mode` of ack faithfully reflects whether `resume` or `snapshot` is issued.
   Fixed a real bug in the same batch: resync will return a subscription that is not recognized by the host.
   "With ack, zero frame", and renderer's `conversationProjectionStore.awaitingInitial`
   To wait for the initial frame - now consistent with TS `v4-gateway.ts`, throw it directly
   `fault.subscription.notOwned` (the token is in the message body, the client presses it to heal itself).
   Two TS accounting details that have not yet been ported (two-phase commit/rollback of frame reservation,
   wire-snapshot byte upper limit reversely raised `floorSeq`) recorded in
   Difference table between `specs/disabled-features.md` §4.5 and `native/crates/zcode-host/README.md`.
9. **Plan mode representation differences** (2026-10-29): TS builds plan into `planEnabled` that is orthogonal to `mode`
   Boolean, `session-mode-port.ts` only changes this Boolean, `getPrePlanMode()` is always undefined; Rust host
   Use the existing expression of this crate `collaboration_mode == "plan"` (the goal loop and the goal/plan mutual exclusion guard all follow this
   Determination), so record the original mode into `meta.plan_previous_mode` before entering, and restore it when exiting. The two sides of the projection are the same shape:
   `config.mode` + `config.planEnabled` (TS’s own criterion is `payload.planEnabled ?? mode === "plan"`)
   + `config.planTransition` (only written when the tool is driven, the desktop composer's plan switch draft is synchronized according to it;
   Integer keys are omitted by default instead of writing `null` - `sessionConfigStateSchema` is `.optional()` for it,
   `null` will cause the entire snapshot to fail to parse). The contract between rules, gate ranking and approval semantics is
   `specs/rust-native-crates.md` S3 "plan mode"; remaining differences (`allowedInPlanMode` capability bit undeclared,
   Failure to write the plan file does not revoke approval) in README section 12a.