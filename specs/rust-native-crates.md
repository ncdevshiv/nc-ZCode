# Spec: Native crate map, sidecar protocol, stage acceptance

Owner: pending (architecture)
Status: **updated 2026-11-06** — S0–S5 executed 2026-09-22 (evidence RUST-PORT-LOG ENTRY 005 as of that date); S3 host now `nc-code-host` (198 files, 43 tools, V4 31/31); S5 server live via `nc-code-app` + `channel_router` (8 routes) + `nc-code-shell/src/backend.rs`; S6 in progress (Tauri live, `dev:desktop`→`dev:tauri`). Prior "S6 not started / 12 zcode-* crates" wording superseded; legacy `zcode-*` names are aliases for `nc-code-*`.
Date: 2026-09-22 (doc sync 2026-11-06)
<!-- trace:2026-11-06 docs sync — workspace `members=["crates/nc-code-*"]` native/Cargo.toml:7 (glob-counted 54 crates). Host deps nc-code-host/Cargo.toml:50-60; shell deps nc-code-shell/Cargo.toml:33-44. RUST-PORT-MATRIX dated 2026-09-23 snapshot retained as history below. -->

Governing specs: `specs/rust-pure-native-roadmap.md` (stage definitions, §7 R1–R4),
`specs/rust-cli-provider-boundary.md` (S2 seam/event order), `specs/rust-serialization-ffi-boundary.md`
(encoding/process-boundary rules). This spec only defines the crate-level map, the
sidecar stdio protocol, cross-crate dependency direction, and per-stage acceptance.

## Crate map (stage → crate)

| Stage | Crate (`native/crates/`) | Kind        | Mirrors / replaces                                                         |
| ----- | ------------------------ | ----------- | -------------------------------------------------------------------------- |
| S0–S1 | `nc-code-v4-wire`, `nc-code-core`, `nc-code-rpc`, `nc-code-channels`, `nc-code-cdp` | libs | V4 wire codec, admission/lease, channel RPC wire + registry                |
| S1    | `nc-code-sidecar-proto` + `nc-code-watch/scan/proc/image/diff` | libs + bins | 5 islands: NDJSON framing + fileWatcher/fsWalker/taskkill/jimp/jsdiff     |
| S2    | `nc-code-provider` | lib | `adapters/src/model/**` (ModelExecutor semantics)                          |
| S3    | `nc-code-host` | lib + bin (`zcode-host`) | 198 `.rs` files / 83,922 lines; 43 tools (32 sync + 11 async, `src/tools/mod.rs:258-369`), V4 31/31 (`src/rpc.rs:56`) |
| S3–S5 | `nc-code-workflow`, `nc-code-services-*` (35 crates), `nc-code-service-core`, `nc-code-node-repl` | libs | run journal/project-driver, per-domain services, node_repl host-edge       |
| S4–S5 | `nc-code-app`, `nc-code-server`, `nc-code-shell` | libs + bins | composition root (registry) + axum `channel_router` (8 routes) + Tauri shell w/ real backend (`src/backend.rs:66,93`) |
Full member list: glob `crates/nc-code-*` (54 crates counted 2026-11-06). |

Dependency direction (enforced by review; no exceptions without updating this spec):

- Island bins (`watch`/`scan`/`proc`/`image`/`diff`) → `nc-code-sidecar-proto` only.
- `nc-code-host` deps (actual, `nc-code-host/Cargo.toml:50-60`): `nc-code-core`,
  `nc-code-v4-wire`, `nc-code-provider`, `nc-code-scan`, `nc-code-proc`,
  `nc-code-workflow`, `nc-code-node-repl`, plus `nct-mcp`/`nct-core` (external
  `G:/nc-sdk/nc-tools` kernel, 89 typed adapters, `src/nc_tools.rs:1`).
- `nc-code-shell` deps (actual, `nc-code-shell/Cargo.toml:33-44`): `nc-code-cdp`,
  `nc-code-host`, `nc-code-app`, `nc-code-server`, `nc-code-service-core`, plus
  browser/computer platform crates. Composition root `nc-code-app` assembles all
  real channels (no placeholders).
- Workspace deps (`native/Cargo.toml:10-15`): `serde`, `serde_json`, `tokio`. Crate-specific
  deps (notify, image, windows, axum, reqwest, tauri, …) live in the crate's own
  `Cargo.toml`.

## Sidecar stdio protocol (S1 islands)

Transport: child process stdin/stdout, **NDJSON** (one JSON object per `\n`-terminated
line), stderr reserved for diagnostics. Same process-boundary encoding family as the
existing v4 NDJSON links (see serialization spec); no new binary framing.

Frame shapes (`zcode-sidecar-proto`, protocol version `1`):

```jsonc
{"v":1,"id":7,"method":"watch","params":{...}}      // request  (client → sidecar)
{"v":1,"id":7,"ok":true,"result":{...}}             // response success
{"v":1,"id":7,"ok":false,"error":{"code":404,"message":"..."}} // response failure
{"v":1,"event":"fs_event","data":{...}}             // async event (sidecar → client)
{"v":1,"event":"ready","data":{"sidecar":"zcode-watch","proto":1}} // first line on start
```

Rules:

1. Every frame carries `v`. Version mismatch → error frame
   (`code: 400 PROTOCOL_VERSION_MISMATCH`) then the sidecar exits(1). Fail closed.
2. Malformed line → `event:"proto_error"` (raw line truncated to 512 bytes) and the
   sidecar **keeps serving**; it never panics on client input.
3. `method:"shutdown"` → success response, then flush, then exit(0). EOF on stdin →
   same graceful path. Double shutdown is idempotent.
4. Events may interleave any request/response order; correlation is by `id` only.
5. Error codes: 400 protocol/params, 404 unknown method, 409 state conflict
   (e.g. duplicate watch id), 500 internal. Sidecar-internal failures are reported
   as error responses or `scan_error`-style events per method spec — never silent.

## Per-stage acceptance (what "done" means here)

- **S1**: each island = lib (unit-tested) + bin (stdio loop) + `tests/e2e_stdio.rs`
  spawning the real binary via `env!("CARGO_BIN_EXE_*")`. Diff parity proven against
  **fixtures generated by running the real npm `diff@9`** from the repo's node_modules.
  Image parity is **semantic** (dimensions/format/fit math), not byte-identical —
  jimp and `image` encoders differ; recorded as an accepted divergence (this
  deviates from the roadmap's blanket "byte-identical" line for the codec only;
  watcher/scan/diff/process outputs must be byte-equal where the TS side is
  deterministic).
- **S2**: SSE parser passes chunk-boundary torture tests; streaming tool-call
  assembly, retry/budget/idle/empty-completion/off-peak classification, strict-schema
  folding, and `video/*` request shape are covered by tests derived from the TS
  sources and `patches/@ai-sdk__*`. **Live-provider golden-request corpus from
  `runner-debug` recordings is unavailable in this checkout** — that gate stays
  OPEN and is reported as not met, not simulated.
- **S3**: `CommandInbox` semantics (serial admission, monotonic `admissionSeq`,
  idempotent resubmit, cancel) and owner/lease rules (acquire/renew/steal, stale-run
  guard) pass concurrency tests with an injected clock — no wall-clock sleeps in tests.
- **S3 `nc-code-host` (updated 2026-11-06; supersedes six-tool wording)**: the Rust host binary
  speaks the existing v4 stdio NDJSON RPC surface (method names and envelope shapes mirror
  `V4_METHODS`/`V4_NOTIFICATIONS` in `packages/shared/src/zcode-protocol-v4/transport.ts`),
  integrating wire codec + admission + provider executor **in-process** — no sidecar hop.
  Current state: **43 tools** (32 sync + 11 async, `src/tools/mod.rs:258-369`) stacked on
  nc-tools kernel 89 typed adapters (`src/nc_tools.rs:1`), **V4 31/31 wired**
  (`src/rpc.rs:56` empty), workflow 7 reads dispatched (`src/lib.rs:906-955`) + 4 commands
  via `commands/workflow_commands.rs`. Both delivery profiles (`continuous`/`replayable`)
  implemented from shared parameters; terminal states byte-identical.
  Acceptance status: host 198 files, e2e stdio + tool-loop + lifecycle suites green
  (see host README 验证). Explicitly **OPEN**: runner-debug live golden corpus,
  `generateText` parity, Linux/macOS CUA/CDP where shipped `capability_unsupported`
  (honest named error), sqlite single-writer parity gaps per host README diffs 13–41.
- **S4**: `nc-code-shell` (32 `.rs` files) live — Tauri commands + embedded backend
  (`src/backend.rs:66` build + `:93` serve on `127.0.0.1:0`). Interactive window
  verification and the R4 BrowserView gate remain governing OPEN items per roadmap §7.
- **S5**: axum `channel_router` live — 8 routes (4 base + 4 `/api/remote/*`) behind real
  `nc-code-app` registry; `ProcessLivenessHost` honesty default retained for the
  unmanaged session-host seam only. `/ws` channel RPC is live (`src/lib.rs:286`).

## Cutover order and `old.back/` staging (S3→S6, updated 2026-09-22 ENTRY 010)

Deletion is **stage-gated**, never big-bang: the desktop app and `zcode --web`
spawn the TS CLI today, so moving any still-executing TS module would break the
running product. A TS module may move to `old.back/` only when **all four** hold:

1. its Rust counterpart is feature-complete for the product surface that uses it
   (not just unit parity — the real command path must exist in Rust);
2. conformance/parity tests are green (golden vectors, e2e stdio, fixture parity);
3. the per-workspace flag seam actually routes production traffic to the Rust
   path, and the TS path still runs green with the flag off (rollback intact);
4. after the flip, the TS module has zero remaining importers in the workspace
   (verified by typecheck + knip), so the move cannot break `bun run build`.

Staged order (each stage ends with its own `old.back/` move or an explicit
"holds" decision):

1. **Agent host + loop** (this increment): `zcode-host` gains the tool layer,
   context assembly, tool-calling loop, compaction. Cutover = new headless entry
   `zcode --native` (additive; TS untouched).
2. **Provider executor**: per-provider flag flip of `zcode-host`'s model path;
   `adapters/src/model/**` moves to `old.back/` per mirrored module once the
   parity corpus is green (live corpus still OPEN — flip stays flag-gated).
3. **S1 islands** (watch/scan/proc/image/diff): wire the sidecars into the TS app
   first (TS keeps ownership), verify, then move `fileWatcherService.ts`,
   `fsWalker.ts`, `taskkill` call sites, jimp hot path, jsdiff call sites.
4. **Server**: `zcode-server` behind the web entry flag; then `packages/server`.
5. **Shell**: Tauri replaces Electron main per-feature (R4 buckets); then
   `packages/desktop/src/main`.
6. **CLI/SEA**: `zcode` native binary; delete `build:sea`/postject scripts.
7. **S6 exit**: delete `old.back/` after the parity soak window; Node leaves
   packaging.

`old.back/` lives at the repo root, is gitignored-adjacent (tracked, not
built), and preserves each moved module's original path under it so parity
diffs and rollbacks stay one `git mv` away.

### Stage status (2026-11-06; 2026-09-22 history retained)

- **Stage 1 (agent host + loop): landed + expanded.** `nc-code-host` (binary
  `zcode-host`) runs the multi-round agent loop with **43 tools** (32 sync + 11
  async) + nc-tools 89 adapters; both provider wire shapes live-verified (ENTRY 008/009).
- **Stage 3 first island (diff): wired, flag-gated.** TS package
  `apps/zcode-cli/packages/sidecar` (`@zcode/sidecar`) bridges the NDJSON stdio
  protocol (ready handshake, request/response, pool with idle reaping and
  crash eviction). `ZCODE_NATIVE_SIDECARS=diff` routes the four jsdiff call
  sites (tool/diff, turn-file-changes, file-rewind, cold-file-change-summaries)
  through the Rust sidecar; unavailability degrades to jsdiff with a warning,
  protocol/handler errors surface. `diff` is no longer a runtime dependency of
  core/bootstrap — jsdiff is contained in the adapter. Parity: 87 real-jsdiff
  fixtures replayed through the real binary, 88/88 tests green. Five sidecar
  binaries are built in release and staged into
  `packages/desktop/bundled-tools/<platformKey>/sidecar/` on the native-search
  runtime-asset chain (skippable with `ZCODE_SKIP_NATIVE_SIDECARS=1`); desktop
  main resolves that directory (or `resources/tools/sidecar` in a packaged
  app) and injects `ZCODE_SIDECAR_DIR`. `ZCODE_NATIVE_SIDECARS_OFF=1` is the
  global kill switch. Verified: the default-on path (flag unset) resolves the
  staged **release** binary and returns jsdiff-identical results.
- **Stage 3 all five islands: wired (ENTRY 012), 4-on since scan/watch flip.**
  proc / scan / watch / image facades sit behind the same bridge with TS fallbacks;
  image default-ON (measured semantic parity). Current default
  `DEFAULT_ON_ISLANDS=["diff","image","scan","watch"]` (`sidecar/src/config.ts:44`);
  `proc` default-off (MCP stdio soak). diff 88/88 byte parity retained.
- **S6 status (2026-11-06): in progress, not "not started".** Tauri is live
  (`dev:desktop`→`dev:tauri`, `package.json:15`), embedded Rust backend real
  (`nc-code-shell/src/backend.rs:66,93`), `old.back/` still zero-move (four
  conditions unmet — no deletion yet). S6 exit (delete `old.back/`, Node leaves
  packaging) remains gated on parity soak + zero-reference proof.

## Third-party notice rule for cargo

Any change to `native/**/Cargo.toml` or `native/Cargo.lock` must regenerate cargo
notices in the same change: `node scripts/third-party-cargo.mjs --write` (collects
license texts from the cargo registry into `third-party/native-cargo/`), then
`node scripts/licenses.mjs notices` (rebuilds `THIRD-PARTY-NOTICES.md` +
`third-party/inventory.json`). The generator fails if `Cargo.lock` and the recorded
inventory disagree. Scope mirrors `third-party/native-search/sources.json`: the whole
lockfile conservatively, including dev/test/build crates.

## Verification commands

```sh
cargo fmt --manifest-path native/Cargo.toml --all -- --check
cargo clippy --manifest-path native/Cargo.toml --workspace --all-targets -- -D warnings
cargo test --manifest-path native/Cargo.toml --workspace
bun run golden:check && bun run typecheck && bun run lint && bun run fmt:check
bun run architecture:check --changed
node scripts/third-party-cargo.mjs --check   # cargo notices fresh
```

## Chrome import (2026-11-04)

Owner: `nc-code-shell`; the command surface (`import_chrome_browser_data`) is unchanged.

- Windows: `Network/Cookies` + DPAPI `Local State` key + `v10`/`v11` AES-GCM,
  CDP `Network.setCookie`; Local Storage via temp Chrome + `DOMStorage` (unchanged).
- macOS: Keychain `Chrome Safe Storage` (`security find-generic-password`),
  PBKDF2-HMAC-SHA1 1003 iterations, AES-128-CBC with the 16-space IV, Tauri
  `WebviewWindow::set_cookie`.
- Linux: same CBC path with Chrome's documented `peanuts` fallback (1 iteration),
  matching `chromeCookieManager.ts`.
- Chrome cookie schema 24+ strips the 32-byte host digest from the plaintext.
- Local Storage on macOS/Linux: explicit `chrome_local_storage_import_failed`
  (`originsFailed` counts the discovered origins); Tauri has no cross-origin
  DOM-storage write API on WKWebView/WebKitGTK.
- Verification: Windows tests cover PBKDF2 RFC 6070 vectors, an OpenSSL AES-CBC
  vector, the `v10`/schema-24 framing, keychain exit mapping, and the existing
  DPAPI DB test. The Linux branch compiles against real `rusqlite`/`cookie`/
  `aes`/`cbc` crates in WSL; the macOS keychain branch is API-checked only.

## Print-to-PDF (2026-11-04)

Owner: `nc-code-shell`; the command surface (`print_page_to_pdf`) is unchanged.

- Windows: WebView2 `PrintToPdf` (unchanged).
- macOS: `WKWebView.createPDFWithConfiguration:completionHandler:` with `nil`
  configuration; `respondsToSelector` guard returns `print_unsupported` below
  macOS 11. Completion handler → `CFData::to_vec` → `%PDF-` validation.
- Linux: `WebKitPrintOperation` with GTK `PrintSettings` (`Print to File`,
  `output-uri`, `output-file-format=pdf`); `print()` runs without a dialog and
  the produced file is read back and `%PDF-`-validated.
- Failure contract: empty/non-PDF bytes → `print_failed`; missing output file →
  `print_failed`; macOS < 11 → `print_unsupported`.
- Verification: Windows build/tests green; the Linux branch compiles against
  real `webkit2gtk 2.0.2` + `gtk 0.18` in WSL; the macOS branch is API-checked
  only (no Apple SDK on this host). A full Linux app build still needs the
  Tauri system packages (`libayatana-appindicator3-dev`, `librsvg2-dev`,
  `libxdo-dev`, `libssl-dev`).

## Native browser-use tool (2026-11-04)

Owner: `nc-code-host` owns the tool contract and port trait; the composition root
(`nc-code-app`) injects the implementation; `nc-code-shell` owns the CDP bridge.

- Surface: one `Browser` tool whose input is a `BrowserCommand` JSON
  (`method` + command fields). Methods: `navigate`, `back`, `forward`, `reload`,
  `getState`, `snapshot`, `evaluate`, `screenshot`, `click`, `fill`, `type`,
  `press`, `scroll`, `waitFor`, `list`.
- Port: `nc_code_host::tools::browser::BrowserControlPort` with
  `list()` and `execute(command)`. Absent port = `browser_unavailable`
  (fail-closed); the tool never fabricates a page result.
- Dependency edges: `nc-code-app` and `nc-code-shell` reference only the port
  trait (`nc_code_host::tools::browser::BrowserControlPort`); neither adds host
  business logic. `nc-code-services-agent` passes the port into
  `HostServer::set_browser_port`.
- Read-only methods (`getState`, `snapshot`, `list`) skip the permission prompt;
  every other method requires approval.
- Tauri shell: `ShellBrowserPort` targets the newest visible child webview and
  maps commands to WebView2 `CallDevToolsProtocolMethod`. `snapshot` assigns
  `data-zcode-ref` refs; `click`/`fill` resolve them. macOS/Linux return
  `capability_unsupported` (no CDP endpoint).
- Acceptance: host unit tests cover the contract, read-only classification,
  fail-closed, and input validation; shell tests cover error-code mapping,
  modifier bits, timeouts, result shaping and ref/key helpers. The
  `--browser-selftest` diagnostic runs the real WebView2 path
  (navigate → snapshot → elementInfo → click → fill → evaluate → screenshot →
  getState → host tool wrapper) and passed on Windows 2026-11-04.
- Playwright subset: `playwright`/`playwrightWaitForTimeout` are routed through
  the shell. Locator operations `count`, `allTextContents`, `isVisible`,
  `isEnabled`, `textContent`, `innerText`, `getAttribute`, `evaluate`, `waitFor`,
  `click`, `dblclick`, `fill`, `press`, `setChecked`, `selectOption`,
  `downloadMedia` are implemented over CSS/`css=`/`id=`/`data-testid=`/`text=`/
  `role=`/`xpath=` selectors with `nth=` and `>>` chaining, plus the
  `internal:` selectors the TS client generates
  (`internal:role=/text=/label=/attr=/testid=/and=/or=/has=/has-not=/
  has-text=/has-not-text=/control=enter-frame`). `elementScreenshot`,
  `waitForEvent("download")`, and `downloadPath` are implemented (Windows
  WebView2 `DownloadStarting` COM handler). `fileChooserSetFiles` fails
  explicitly with `capability_unsupported` (IAB file uploads are unsupported by
  the TS product too).
- Not covered: the full Playwright injected selector engine (unknown
  `internal:` selectors and cross-origin frame entry fail explicitly) and the
  `agent.browsers` node_repl bridge (the Rust host has no JS plugin runtime;
  R1/R3 in `rust-pure-native-roadmap.md` track that epic).

## Playwright locator subset (2026-11-04)

Owner: `nc-code-shell` (`browser_playwright.rs`) behind the host `Browser` tool.

- Commands: `playwright {action, tabId?}` and
  `playwrightWaitForTimeout {timeoutMs, tabId?}`.
- Actions: `locator` (the operation set above), `domSnapshot`, `elementInfo`,
  `elementScreenshot`, `evaluate`, `waitForLoadState` (`load`/
  `domcontentloaded`), `waitForURL` (with `*` wildcards), `waitForEvent`,
  `downloadPath`. On Windows each child webview registers a WebView2
  `ICoreWebView2_4::add_DownloadStarting` handler that forces the destination
  into `%TEMP%\zcode-browser-downloads` and records `{id, tab, path}`; the
  handler is registered per creation and the event fires before bytes are
  written, so `downloadPath` polls until the file exists. `fileChooserSetFiles`
  is an explicit `capability_unsupported` (IAB file uploads are unsupported by
  the TS product too); on non-Windows platforms download events report
  `capability_unsupported` because there is no WebView2 handler.
- Selectors: CSS, `css=`, `id=`, `data-testid=`, `text=` (substring; quoted
  exact), `role=<role>[name="..."]`, `xpath=`, `nth=` and `>>` chaining.
- `internal:` selectors match the syntax the TS client emits:
  `internal:role=<role>[name="…"i|s|/re/]` (implicit ARIA roles plus
  aria-label/aria-labelledby/label/alt/innerText accessible names),
  `internal:text=…` (strict `s`/quoted = normalized whole-text equality; `i` =
  case-insensitive substring, leaf-most match),
  `internal:label=…`, `internal:attr=[name=matcher]`,
  `internal:testid=[data-testid=…s]`, `internal:and="sel"`,
  `internal:or="sel"`, `internal:has="sel"`, `internal:has-not="sel"`,
  `internal:has-text=…`, `internal:has-not-text=…`, and
  `internal:control=enter-frame` (same-origin iframes). Unknown `internal:`
  selectors and cross-origin frame entry fail explicitly.
- Actionability: pointer/input operations poll for existence/visibility/
  enabled state up to the action timeout (default 3s), scroll the element into
  view, dispatch trusted CDP mouse/key events, and verify `setChecked`.
- Live `--browser-selftest` evidence: `playwright count: 1`,
  `playwright textContent: "Go"`, `playwright fill: value="locator"`,
  `playwright click: title="clicked"`, `playwright xpath count: 1`,
  `playwright chain count: 1`, `playwright nth count: 1`,
  `playwright elementScreenshot: base64_bytes=600`, internal role/name/text/
  testid/attr/label/and/or/has/has-not/has-text/frame counts all matching, and
  a real loopback `Content-Disposition: attachment` download:
  `playwright download: id=iab-download:… bytes=17 text="selftest-download"`,
  PASS/EXITCODE=0.
- Tests: `browser_playwright::tests` cover the resolver script
  (css/xpath/nth/chaining/internal), JSON embedding of the selector, press
  modifier parsing, URL wildcards, timeout clamping, `waitForEvent` validation,
  and `downloadPath` unknown-id handling; `browser_view::tests` covers the
  tab-scoped download registry (unique ids, per-tab filtering, cross-tab
  lookup); the host Browser tool tests cover read-only classification.

## Native computer-use tool (2026-11-04)

Owner: `nc-code-host` owns the contract (`tools/computer.rs`, `ComputerControlPort`
trait); the composition root injects the platform implementation; `nc-code-shell`
owns Windows capture/input (`computer_control.rs`).

- Commands: `listApps`, `getState`, `screenshot`, `activate`, `move`, `click`,
  `scroll`, `type`, `press`, `drag`, `wait`.
- Observation: `listApps` enumerates visible top-level windows with
  `{windowId, pid, name, title, bounds, clientOrigin, clientSize, foreground}`;
  `getState` returns the foreground window, cursor position and virtual-screen
  rect; `screenshot` captures a window/region/full virtual screen as a real PNG
  (`Page.captureScreenshot` is not involved; this is GDI `BitBlt` + PNG
  encoding), capped at 64 MP and `maxBytes` (default 8 MiB).
- Input: `activate` uses `ShowWindow(SW_RESTORE)` + `SetForegroundWindow` and
  reports `foreground_denied` when Windows refuses; `move`/`click`/`scroll`/
  `drag` use `SetCursorPos` + `SendInput` mouse events; `type` sends UTF-16 code
  units with `KEYEVENTF_UNICODE`; `press` sends a modifier chord + key through
  `SendInput` (key names share `browser_control_scripts::key_fields`).
  `wait` sleeps up to 30 s.
- Fail-closed: absent port -> `computer_unavailable`; non-Windows ->
  `capability_unsupported`; input validation happens in the host before the port
  is touched (coordinates, button/clicks, required text/key, scroll delta, wait
  bound).
- Deferred (2026-11-04): macOS (`CGEvent`/`AXUIElement`) and Linux
  (`XTest`/AT-SPI) backends are not implemented. This host has no Apple SDK and
  no X11 display, so those branches could be neither compiled nor exercised;
  shipping unverified input injection was rejected. The port already returns a
  named `capability_unsupported`, so the model surface stays truthful.
- Read-only classification: `listApps`/`getState`/`screenshot`/`wait` never
  prompt; `activate`/`move`/`click`/`scroll`/`type`/`press`/`drag` require
  approval.
- Live `--computer-selftest` evidence on Windows (real Tauri window, real
  `SendInput`): `listApps windowId=15075744 pid=31244 origin=(128,151)`,
  `activate foreground granted`, click+`type` produced input value `"hello"`,
  button handler set `data-clicked=1`, wheel set `scrollY=300` (`wheels=1`),
  `Tab` accepted, screenshot 17,344 bytes PNG 716x539, drag ended at the target
  cursor `(212,255)`, PASS/EXITCODE=0.
- Tests: `tools::computer::tests` cover the contract surface, read-only
  classification, fail-closed without a port, method validation, command
  validation before the port, and forwarding to the port; shell unit tests keep
  the existing 124+3 suite green.

## Native-layer occlusion (2026-11-04)

The Tauri browser pane is a native child webview above the DOM, so Radix
poppers/menus/dialogs/tooltips/toasts cannot paint over it. The fix keeps the
tab's requested visibility and the overlay state separate:

- Shell command `browser_view_set_occluded {key, occluded}` sets a per-entry
  `occluded` flag and applies `visible && !occluded` to the native webview.
  `browser_view_state` now returns `{visible, occluded, effectiveVisible}` so the
  renderer can observe both. `browser_view_set_visible` also applies the composed
  visibility, so a concurrent show does not override an active overlay.
- Renderer `packages/ui/src/browser-use/browserUseOcclusion.ts` watches
  `data-radix-popper-content-wrapper`, menu/select content, `role=dialog`,
  `role=alertdialog`, `role=menu`, `role=listbox`, `role=tooltip`,
  `data-sonner-toast`, and the explicit `data-browser-occluder` hook. A scan runs
  on open, on DOM mutations, on scroll/resize, and on a 120 ms interval; it only
  reports when a visible overlay rect intersects the pane rect.
- `BrowserViewportSurface` starts the watcher in the Tauri runtime and calls
  `TauriBrowserGuestController.setOccluded`; Electron `<webview>` keeps DOM
  compositing and is unaffected.
- Live `--browser-selftest` evidence: after navigation the selftest hides the
  webview via `apply_occluded`, asserts the native `IsWindowVisible` is false,
  restores it, asserts it is visible again, and prints
  `occlusion: hidden=false restored=true`; PASS/EXITCODE=0.
- Tests: `packages/ui/test/browserUseOcclusion.test.ts` 5/5 (geometry, style,
  selector coverage, watcher flip/restore); shell unit 125 includes the
  `effective_visible` truth table.

## Browser recording (2026-11-04)

Owner: the renderer owns the capture/encode job; `nc-code-shell` owns the
workspace write.

- `recordingStart` creates `iab-recording:<uuid>` in the renderer lifecycle
  controller and starts the capture port. The port polls
  `Page.captureScreenshot` through `browser_view_cdp`, draws frames into a
  canvas and records `canvas.captureStream(fps)` with `MediaRecorder`
  (VP9/VP8 WebM).
- `recordingStatus` returns the job snapshot and accepts a workspace-relative
  `.webm` `outputPath`; without one the artifact defaults to
  `browser-recordings/<job-id>.webm`.
- `recordingCancel` aborts a running job; `close`/`closeSession` cancel the
  matching running jobs.
- Scripted `options.actions` (`wait`, `click`, `type`, `hover`, `move`,
  `scroll`, `scrollTo`, `wheel`, `drag`, `waitFor`, `delayAfterMs`) execute over
  CDP during capture; selectors/text are JSON-escaped. Unless `showCursor:
  false`, a red cursor overlay is injected into the guest page and driven by the
  same CDP mouse events, so it appears in the recorded frames.
- The shell command `save_browser_recording(workspacePath, relativePath, data)`
  validates the relative path and parent canonicalization, requires the WebM
  EBML magic, caps the payload at 256 MiB, and writes atomically.
- Fail-closed: no capture port -> `capability_unsupported`; a second active
  recording on the same tab -> `execution_error`; unknown recording id ->
  `backend_unavailable`.
- Verification: 9 renderer recording tests (fail-closed, actions pass-through,
  cursor/action script escaping, completed artifact write, duplicate tab,
  cancel, invalid output paths, closeSession cancellation, CDP action
  execution); 4 shell `browser_recording` tests; live Tauri probe wrote
  `recordings/probe.webm` with `1A 45 DF A3 ...` and rejected
  `../escape.webm` / non-WebM payloads. The live selftest captures 3 scripted
  actions and verifies a red cursor pixel in the frame.

### Live recording selftest (2026-11-04)

`zcode-shell --browser-recording-selftest` creates a real child webview and a
local selftest page; the page captures the child over `browser_view_cdp`,
drives 3 scripted CDP mouse actions, injects the cursor overlay, encodes WebM
with `MediaRecorder`, writes through `save_browser_recording`, and reports the
artifact back to the shell. The shell verifies the workspace path, byte count,
EBML magic, action count, and a red cursor pixel before PASS.

Live Windows result (WebView2, real MediaRecorder):

```
[browser-recording-selftest] capture: 15 frames, video/webm;codecs=vp9, 3 scripted actions
[browser-recording-selftest] cursor: visible (pixel Some([255, 73, 82]))
[browser-recording-selftest] artifact: ...\recordings\selftest.webm (23294 bytes)
[browser-recording-selftest] PASS
```

`ffprobe` on a kept artifact: `codec_name=vp9`, `width=800`, `height=600`,
`format_name=matroska,webm`, `duration ~1.95s`. Set
`ZCODE_RECORDING_SELFTEST_KEEP=1` to keep the workspace for inspection.
