# Spec: TS-backend deletion remains — controller / remote / PDF / share / shell / cutover

Status: in execution 2026-11-06. Cutover bridge + SSH probe + real-PDF
e2e landed 2026-11-06 (see §§2/3/6 notes below); GUI-window + live-SSH-server
e2e remain manual. Second pass 2026-11-06 (see "Progress" at the end):
`/api/remote/*` HTTP endpoints landed in `nc-code-server`, wrong-range PDF
error uses the real `pdfinfo` count, bridge dedup, `bundle.active=true`,
both headless selftests PASS. Governing specs: `specs/rust-backend-replacement.md`
(done definition §0 + anti-fake §5), `specs/rust-native-crates.md` S3/S4,
`specs/disabled-features.md`, `specs/zai-claim-boundary.md`.

## 0. Goal and definition of done

The Rust backend (stdio `zcode-host` + channel registry via `nc_code_app::build` +
Tauri `zcode-shell`) must cover the six remains below with real implementations
(no `todo!()`, no always-empty stubs) before any TS backend entry point
(`packages/desktop/src/host/index.ts`, `packages/services/*`, `packages/server/*`,
`apps/zcode-cli` runtime paths) is deleted or moved to `old.back/`.

Deletion preconditions (all required):
- `pnpm native:coverage` exits 0 (channel/member + v4 inventory exact complement).
- `cargo test -p nc-code-host` + `-p nc-code-services-client` + `-p nc-code-shell`
  green, `cargo clippy -- -D warnings` green, `cargo fmt --check` green.
- Tauri window boots to `packages/web` UI via `nc_code_app::build` with no
  `Unknown channel` / `Method not found` for the wired surface.
- Desktop e2e (real Electron/Tauri + real model round) green on Windows;
  macOS/Linux deltas explicitly listed as remaining risk.

## 1. v4/controller (3 methods)

Owner: `nc-code-host` stdio surface (this spec) + `nc-code-services-client`
channel `window-controller` (existing local projection).

Product rules:
- `v4/controller/subscribe|resync|unsubscribe` (stdio NDJSON) expose the same
  window-controller topics as TS (`controller/tasks-index`,
  `controller/workspaces`). Local source = the host's own sessions/workspaces.
- `subscribe` params = `ControllerSubscribeParams` (`topic`, optional
  `base {logEpoch, seq}`, optional `visibility`). Response = ACK-only
  `{ack: {subscriptionId, mode: "snapshot", logEpoch}}`; the snapshot frame is a
  **post-response notification** (`v4/controller/frame`, params = controller
  frame JSON with strict keys `topic, subscriptionId, logEpoch, fromSeq(=0),
  toSeq, sentAt, payload`). This mirrors conversation subscribe ordering
  (response line first, frame after).
- `resync` requires an owned `subscriptionId`; unknown id fails closed
  (`CODE_INVALID_PARAMS`, `fault.subscription.notOwned` analogue for controller).
- `unsubscribe` is idempotent (unknown id → `{}` success, never error).
- Snapshot tasks are derived from the host's live sessions at subscribe/resync
  time (real state). Pinned/archived membership is owned by the channel
  `window-controller` (TaskIndexStore); the stdio projection reports default
  membership (`pinned:false, archived:false, active:true`) and `liveStatus`
  from session phase (`completed`/`error`/`idle`), documented as an intentional
  split, never a fake pin/archive fact.
- Remote addresses (`remoteSessionId`) are **not** rejected here; routing is §2.

Acceptance: unit tests for topic validation, ACK shape, snapshot frame shape,
resync unknown-id failure, unsubscribe idempotence; e2e stdio test creates a
session then subscribes and asserts the task appears in the snapshot.

## 2. Remote routing

Owner: desktop window-connection registry, re-invented in Rust.

Product rules:
- TS `windowRemoteConnectionRegistry.ts` semantics are the contract:
  `connect → remoteSessionId`, `bindWorkspaceContext`, `resolveScopedServices`,
  `findSessionForWorkspace` (fails closed on ambiguity), `disposeSession`.
- Rust owner: `nc-code-services-client::remote_registry` state machine
  (connecting/online/closing/failed/disconnected, per-session generation,
  WSL idle TTL, running-task gate). Transports: SSH (pooled by host key),
  WSL (pooled by distro+user), Docker (dedicated session). A remote session's
  `services` = a `TaskService`-shaped facade over a forwarded channel.
- `window_controller.rs` no longer rejects on `remoteSessionId` presence;
  it resolves via the injected registry. Unknown session → named error
  `RemoteSourceUnavailable` ("no such remote session in window registry"),
  which is a routing fact, not "host does not own a registry".
- `nc_code_app::build` constructs the registry and injects it into
  `ClientServices::new(tasks, registry)`.

Acceptance: registry unit tests (connect/bind/resolve/dispose, ambiguity
failure, generation bump on rebind); controller routing tests (remote address
routes to the registered facade; unknown session fails with the named error).

Increment notes (2026-11-06, BackendBridge):
- Registry facade type generalized from `Arc<TaskService>` to
  `Arc<dyn Channel>` (in-process `TaskService` remains a `Channel`; the
  controller already only uses `Channel::call`/`Channel::listen`, so no
  routing-contract change). New `connect_ssh_tunnel_ws`: spawns the SSH
  tunnel (`ssh -N -L`), then opens a WS client to
  `ws://127.0.0.1:<local>/ws` and registers `WsTaskService`
  (`remote_ws_facade.rs`) — a real WS-channel proxy over the tunnel, not an
  in-process `TaskService`. Its `Channel::call` forwards each invoke over WS
  (`encode_request` framed per `nc-code-server` `ws.rs`); `listen` forwards
  `EventListen` and streams `EventFire`. The generic
  `WsChannelClient::invoke(channel, method, arg)` proves `system.info`
  (channel `system`) through the tunneled WS endpoint.
- Local e2e (no sshd on Windows dev boxes): the tunnel is bypassed-with-
  equivalent by `TcpForwarder` (in-process TCP byte pipe — exactly what
  `ssh -N -L` does at TCP level); integration test
  `remote_ws_tunnel_e2e.rs` spawns a real `zcode-server`, probes
  `/healthz` through the tunnel, calls `system.info` + `task/listTasks`
  over the WS facade, and proves `stop`/dispose closes the tunnel port
  (no listener leak) and kills a real tunnel `Child` (kill-tree reaped).
  Live sshd remains an environment gate (§2 OPEN preserved for sshd
  hosts); the WS-facade code path is identical once the tunnel exists.

OPEN (not in this increment): live SSH/WSL/Docker forwarding to a remote
`zcode-server` (today `remote_discovery` only probes local targets). The
registry + controller routing land first with in-process facades; transport
plugs in behind `connect` without changing the routing contract.

## 3. PDF pages (Read `pages`)

Owner: `nc-code-host` `Read` tool (`tools/read_pdf.rs` + `fs_tools.rs` wiring).

Product rules (mirrors TS `read-pdf.ts` + `adapters/pdf/index.ts`):
- `pages` (e.g. `"1-5"`, `"3"`, `"1-"`) is parsed 1-indexed; `"1-"` means
  to-end but capped at 20 pages/request. Anything else →
  `InvalidInput` with the TS wording.
- Whole-PDF native read (`pages` omitted) keeps the `%PDF-` magic check and the
  20 MiB cap; **additionally** when `pdfinfo` is installed and reports
  `Pages: N`, `N > 10` fails closed asking for `pages` (TS `PDF_TOO_MANY_PAGES`
  analogue). When `pdfinfo` is absent/fails, native read proceeds (TS swallows
  the probe failure the same way).
- Pages read: `pdftoppm -jpeg -r 100 -f FIRST -l LAST <pdf> <tmp>/page`
  (120 s timeout, 5 s availability probe `pdftoppm -v` with caching of the
  positive result only). Rendered `page-N.jpg` files (page number from the
  `-(\d+)\.jpg` suffix) are read, each image-resized via the `image` crate
  (max dimension 2000, max base64 5 MiB / ~3.75 MiB raw — same budgets as
  `ReadImage`), and returned as
  `{type:"parts", filePath, numParts, originalSize, pages:[{type:"image",
  pageNumber, base64, mimeType:"image/jpeg", originalSize, transformedSize,
  resized, compressed, dimensions}]}` sorted by page number.
- Error mapping (stable, no text parsing beyond documented Poppler stderr
  classes): password → `ExecutionFailed`; wrong-range (stderr carries
  `last page (N)`) → page-out-of-range with document count and a suggested
  range; `I/O Error`/`Permission Error` on the input file → I/O/permission;
  damaged/corrupt/invalid or broken-trailer → corrupted; timeout → timeout;
  `pdftoppm` missing → configuration error telling the user to install
  poppler-utils; cancelled (turn cancel) → `Cancelled`.
- Cancellation and timeouts kill the process tree (`nc_code_proc::kill_tree`).

Acceptance: unit tests for range parsing and `pdfinfo` output parsing;
integration test writes a minimal `%PDF-` file and asserts (a) native read
returns the base64 payload, (b) `pages` with a malformed range fails, (c)
`pages` with a valid range either renders (poppler present) or fails with the
poppler-missing configuration error (poppler absent) — never the old generic
"not available in this Rust host" rejection.

## 4. Share attachments (`v4/conversation/attachmentRead|Stat`)

Owner: `nc-code-host` `attachment.rs` + `rpc.rs` inventory.

Product rules:
- These are the **share read** surface (`conversation-share` preflight/publish).
  They share the same content-addressed store and the same row/index
  authorization as `v4/attachment/read`, but unlike preview read they accept
  **any MIME** (text/plain etc. for share selection) and require both
  `target {rowId, entityId}` + `attachmentIndex` as a pair.
- `attachmentRead` params `{sessionId, ref, target, attachmentIndex, offset,
  limit(1..=512 KiB)}` → `{dataBase64, mediaType, totalBytes, nextOffset}`.
  Unauthorized ref → `fault.attachment.previewRefNotAuthorized` (same reason
  code as preview read; the share path does not leak existence).
- `attachmentStat` params `{sessionId, ref, target, attachmentIndex}` →
  `{mediaType, totalBytes, mtimeMs?}` metadata-only (no full-byte read into the
  response). `totalBytes` may exceed the preview cap (stat must express
  over-cap sizes so selection can block early).
- Both methods are removed from `V4_METHODS_UNIMPLEMENTED` and wired in
  `HostServer::handle_request`.

Acceptance: unit tests for unauthorized vs authorized read/stat, cross-session
isolation, offset/limit slicing + `nextOffset` null at end, stat-vs-read parity
(same mediaType/totalBytes).

## 5. Tauri shell parity

Owner: `nc-code-shell`.

Product rules:
- Browser CDP bridge: `browser_view_cdp` (WebView2 `CallDevToolsProtocolMethod`
  on Windows; explicit `browser_view_cdp_unsupported` on macOS/Linux) + the
  `ShellBrowserPort` → Rust host `Browser` tool path is the Tauri
  replacement for the TS `browserControlMainBridge`. macOS/Linux CDP absence
  is an explicit unsupported result, never a silent success.
- Interactive-window verification: `--browser-selftest` (child webview +
  navigate/snapshot/click/fill/evaluate/screenshot via the real CDP path) is
  the GUI verification gate; headless CI only runs `cargo check/build/test`.
- `packages/web` static pipeline: `backend.rs` serves `packages/web/dist`
  (dev) or the Tauri resource `web/` (packaged, `frontendDist` scaffold only
  as initial page). The R4 gate (`main.rs` OPEN note) closes when the
  `--browser-selftest` + window-boot evidence lands on Windows; the build
  check (`backend.rs` index.html presence) stays fail-closed with the
  `vite build` hint.

Acceptance: `cargo test -p nc-code-shell` (permission-list drift test),
manual `--browser-selftest` PASS on Windows recorded in the shell README,
packaged-app smoke (window navigates to `http://127.0.0.1:<port>/`).

## 6. Cutover

Owner: `packages/desktop` (Electron main) + Rust (`nc_code_app::build`).

Product rules:
- Per-window host: replace `utilityProcess.fork()` TS host
  (`packages/desktop/src/host/index.ts`) with a Rust `zcode-host` stdio spawn
  (same workspace/data-dir/provider-handoff contract as `main.rs` today).
- Delegation deletion (`packages/services/*`, `packages/server/*`, `apps/zcode-cli`
  runtime) only after §§1–5 are wired via `nc_code_app::build` and the
  channel registry covers every `service-channels.json` member (or cites a
  spec-backed boundary). No flag flip before evidence.

Acceptance: `ZCODE_NATIVE_HOST=1` desktop e2e (create session → send prompt →
model round → stream frames → stop → restart recovery all from the native
host) green; `pnpm native:coverage` 0; ratchet tightens in step.

## 7. Invent-from-scratch mandate

Anything missing in Rust is **invented and built from scratch in Rust**
(new crates/modules/tests/docs), not left as "no Rust host implementation"
and not shimmed through TS. The registry (§2), controller projection (§1),
PDF pipeline (§3), and share read (§4) in this spec are themselves
invent-from-scratch implementations where no Rust code existed.

## Progress — 2026-11-06 second pass (code-verified)

- **§2 remote server side DONE (was missing):** `nc-code-server/src/remote_api.rs`
  (`POST /api/remote/connect|bind|dispose`, `GET /api/remote/sessions`).
  `ServerOptions.remote: Option<Arc<RemoteConnectionRegistry>>` (conditionally
  registered; absent → JSON 404 with other `/api/*`); wired in `zcode-server`
  `main.rs` and shell `backend.rs`/`window_selftest.rs` from
  `backend.client.remotes` (the window registry the controller routes through).
  `connect` accepts `ssh` (`target.host`, `target.username` → `user@host`,
  **required** `target.remotePort`; runs the tunnel on a blocking worker and
  opens a real WS facade) and `ws` (`target.wsUrl` pre-forwarded endpoint);
  `wsl`/`docker` and unknown kinds fail closed (`"bind-based ... register via
  /api/remote/bind"`), never guessed. Registry `-L` forward target fixed to
  `127.0.0.1` so `user@host` destinations work. `capabilities.remoteAccess`
  advertised in `/api/server-info`.
  Evidence: `nc-code-server/tests/remote_api.rs` 4 passed (validation
  fail-closed, live ws round-trip connect→resolve→sessions→bind→dispose
  against a real `zcode-server`).
- **§3 PDF error mapping DONE:** `render_pages` (via `job_spawn`, stderr
  discarded) now proves empty-output errors from `pdf_page_count`: range past
  the document → `PageOutOfRange` ("Requested page 10 is outside the document
  (PDF has 1 pages)…"), 0 pages → `Corrupted` (empty page tree), unknown
  count → generic `Corrupted`. `classify_render_error` remains the stderr
  classifier for output-capturing callers (doc string updated). Real-PDF e2e
  (`pdf_pages_e2e.rs`, poppler 25.07.0) proves both branches:
  `rendered numParts=1 base64_len=21948` and the wrong-range branch above.
- **§5 shell:** `tauri.conf.json` `bundle.active=true` (+ `config_smoke.rs`
  pin); `[window-selftest] PASS: origin=http://127.0.0.1:62315/ channels=39`;
  `[browser-selftest] PASS` on Windows. `frontendDist` stays `./dist`
  (scaffold initial page, immediately replaced by the embedded backend origin
  per `backend.rs` design).
- **§6 bridge dedup DONE:** `rustChannelBridge.ts` imports the single
  `bridgePort` from `bridgeForward.ts` (inlined duplicate removed). The TS
  bridge is **unchanged** otherwise; `bridgeForward.e2e.mjs` still PASS
  (`system.info` via MessagePort bridge → real `zcode-server`).
- **§6 deletion still gated (correctly):** the 3 bridge TS files
  (`host/bridgeForward.ts`, `host/rustChannelBridge.ts`, `main/rustHostSpawn.ts`)
  remain outside the `ts-frontend-only` allowlist — `architecture:check`
  flags them NEW. They are Electron `utilityProcess`/main shims, NOT
  renderer-moveable; the policy path is `languageBoundary.escapeHatch`
  (`reason`/`approvedBy`/`expires`) — **owner approval required, not
  self-approved**. GUI-window e2e and live-sshd e2e remain manual gates, so
  `packages/services/*`, `packages/server/*`, `apps/zcode-cli`,
  `packages/desktop/src/host/index.ts` stay in place and the fork gate
  (`ZCODE_NATIVE_HOST=1`) keeps falling back to TS.
- Regressions this turn: `nc-code-host --lib` 585 passed, `nc-code-services-client
  --lib` 27 passed, `nc-code-server --tests` (router 10, remote_api 4, ws
  1, e2e 1, lib 6), `nc-code-shell --test config_smoke` 3 passed; all edited
  files <400 lines.
