# Browser-use tab lifecycle (renderer-owned)

Status: implemented 2026-11-04.

## Ownership

The renderer side-pane state machine (`packages/ui/src/lib/workspaceSidePane.ts`)
is the single source of truth for which browser-use tabs exist, which one is
active, and which workspace/session/remote scope owns each tab. The shell keeps
webview authority (`browser_view_*`) and the agent's CDP control plane.

`packages/ui/src/browser-use/browserUseLifecycle.ts` exposes the lifecycle
commands without reaching into React state:

- `listUserTabs()` — browser-use tabs in the current scope as
  `{tabId, url, title, viewport, active, lifecycle?}`; `lifecycle` is omitted
  while the tab is active (matches the TS manager summary).
- `activateTab(tabId)` — activates the tab and reveals the side pane; the
  existing `UnifiedBrowserView` visibility flow then shows the child webview.
- `claimTab(tabId)` — claims a user tab once, activates/reveals it, and returns
  its summary. A second claim fails with `backend_unavailable` until the tab is
  released by `finalizeTabs({status: "deliverable"})`.
- `newTab()` — creates a renderer-owned browser-use tab (`browser-<uuid>`) for
  the current session, activates/reveals it, marks it claimed, and returns its
  summary. The child webview is created when the pane mounts the tab.
- `finalize({tabId?, deliverable?})` — single-tab finalize: `deliverable` by
  default, `active` when `deliverable: false`; uses the active tab when `tabId`
  is omitted. It neither closes the tab nor releases the claim (TS parity).
- `nameSession(name)` — stores the trimmed session name for the current scope
  (`snapshot().sessionName`); empty names return `invalid_input`.
- `closeSession()` — releases every tab in scope back to the user (clears
  lifecycle/claim state) and clears the session name; tabs stay open, matching
  the TS manager's `closeSession`/`releaseToUser` behavior.
- `finalizeTabs({keep})` — marks kept tabs `handoff`/`deliverable`; tabs outside
  `keep` stay open and untouched (same as the TS manager). Unknown keep ids fail
  the whole call with `backend_unavailable` and no lifecycle change.
- `markDeliverable(tabId)` / `markHandoff(tabId)` — set the tab lifecycle and
  return the updated summary. `markDeliverable` does **not** release the claim;
  only `finalizeTabs` deliverable releases it (`releaseToUser` parity).
- `close(tabId)` — removes the tab, clears its lifecycle/claim state, and calls
  `platform.browserViewCloseTab({tabId, workspaceKey, remoteSessionId, sessionId})`
  so the shell releases the same webview. The shell's
  `onBrowserViewCloseTab` event is idempotent after the renderer removal.
- `snapshot()` — the tab-state sync payload
  `{workspaceKey, sessionId, sessionName?, tabs}`.

Scope matching is exact on `workspaceKey`, `remoteSessionId` (empty string
normalized), and `sessionId`; a tab from another workspace/session is never
claimed, activated, finalized, marked, or closed by a mismatched scope.

## Recording

`recordingStart` / `recordingStatus` / `recordingCancel` are renderer-owned jobs
(the webview pixels belong to the renderer/shell, not the agent host):

- `recordingStart({options?, tabId?})` creates an `iab-recording:<uuid>` job and
  returns its `running` snapshot. The renderer capture port polls
  `Page.captureScreenshot` through `browser_view_cdp`, draws frames into an
  offscreen canvas and records it with `MediaRecorder` (WebM VP9/VP8). Jobs are
  per-tab: a second start on the same tab returns `execution_error`.
- `recordingStatus(recordingId, {outputPath?, tabId?})` returns the job snapshot.
  `outputPath` is workspace-relative, must end with `.webm`, and must not escape
  the workspace. When capture completes, the bytes are written through
  `saveBrowserRecording`; without `outputPath` the default is
  `browser-recordings/<job-id>.webm`. The artifact carries the real
  `path/mimeType/width/height/fps/durationMs/frameCount`.
- `recordingCancel(recordingId, {tabId?})` aborts a running job and returns a
  `cancelled` snapshot; `close`/`closeSession` cancel the matching running jobs.
- Without a capture port (web build, Electron preload, remote workspace, or a
  platform without `MediaRecorder`), `recordingStart` fails explicitly with
  `capability_unsupported`.
- Scripted `options.actions` are executed during capture: `wait`, `click`,
  `type`, `hover`, `move`, `scroll`, `scrollTo`, `wheel`, `drag`, `waitFor`,
  including `delayAfterMs`; selectors are embedded with JSON escaping. Actions
  dispatch through CDP (`Input.dispatchMouseEvent`/`mouseWheel`,
  `Runtime.evaluate`), so they appear in the recorded frames. Cancellation stops
  the action loop with `RecordingCancelledError`.
- Unless `showCursor: false`, a red cursor overlay is injected into the guest
  page (`__zcode_browser_recording_cursor`); CDP mouse events drive it, so it is
  visible in the captured frames. The overlay is removed on stop/cancel/failure.
- The shell command `save_browser_recording` validates the relative path,
  canonicalizes the parent inside the workspace, requires the WebM EBML magic,
  and writes atomically (`*.webm.part` → rename).

## Sync

- URLs: `BrowserUseSidePaneContent` records the last reported URL via
  `setBrowserUseTabUrl(tab.tabId, url)` and clears it on unmount.
- Viewports: `useResponsiveBrowserViewportControl` records
  `setBrowserUseTabViewport(tabKey, viewport)` before the
  `browserViewUpdateViewport` round-trip and clears it on unmount.
- `useAppPanels` publishes a snapshot on side-pane state changes, URL changes,
  viewport changes, and registers the live controller via
  `registerBrowserUseLifecycleController` for a host bridge.
- Unknown viewport is the explicit `{width: 0, height: 0}` sentinel; no size is
  fabricated.

## Acceptance

`packages/ui/test/browserUseLifecycle.test.ts` (run from `packages/ui` with
`node --import tsx --test test/browserUseLifecycle.test.ts`) covers:

1. scope filtering (workspace/session/remote), URL/title/active mapping,
2. activation without touching other tabs,
3. cross-workspace/session tabs are not addressable,
4. close removes the tab and picks the fallback active tab,
5. controller reveal/`closeShellTab` calls and snapshot publishing,
6. snapshot publish/subscribe + unsubscribe,
7. viewport sentinel set/clear,
8. controller registry last-writer-wins + disposer,
9. `claimTab` claims once, activates, and rejects a second claim,
10. `markDeliverable`/`markHandoff` update the summary lifecycle,
11. `finalizeTabs` marks kept tabs and leaves other tabs open,
12. `finalizeTabs` rejects unknown keep ids without mutating lifecycle,
13. deliverable releases the claim while handoff keeps it,
14. closing a tab clears its lifecycle/claim state,
15. `newTab` creates an active, already-claimed tab in scope,
16. single-tab `finalize` defaults to deliverable and resets to active,
17. `nameSession` stores the name and `closeSession` releases tabs/name,
18. recording fails closed without a capture port,
19. recording passes scripted actions to the capture port,
20. action scripts escape selectors/text and keep the cursor contract,
21. `recordingStart`/`recordingStatus` writes a completed WebM artifact,
22. a second active recording on the same tab is rejected,
23. `recordingCancel` marks a running job cancelled and calls the port,
24. `recordingStatus` rejects unknown ids and unsafe output paths,
25. `closeSession` cancels running recordings in scope,
26. `executeRecordingActions` drives mouse/fill/scroll/waitFor over CDP.

## Non-goals

Playwright locator actions remain a separate surface; they are not implemented
by this controller.

## Host bridge

- `ShellBrowserPort` delegates `listUserTabs`/`activateTab`/`claimTab`/`newTab`/
  `finalize`/`finalizeTabs`/`markDeliverable`/`markHandoff`/`nameSession`/
  `closeSession`/`recordingStart`/`recordingStatus`/`recordingCancel`/`close` to
  the renderer through `BrowserUseBridgeState`: it emits
  `zcode:browser-use-lifecycle-request` to the `main` webview and waits for the
  `browser_use_lifecycle_result` command with the same `requestId`.
- Bounds: 32 pending requests, 10s timeout. A missing renderer listener returns
  `browser_use_bridge_timeout`; a missing controller (welcome/login screen)
  returns an empty `userTabs` for `listUserTabs` and `backend_unavailable` for
  the other lifecycle commands.
- ACL: the loopback web UI is a remote origin. `capabilities/default.json`
  grants `core:default` + the app permission `shell` to the `main` and
  `resource-manager` webviews only (`remote.urls` = loopback); child browser
  webviews get no IPC. `build.rs` registers the app manifest and
  `tests::command_permission_list_matches_invoke_handler` keeps
  `permissions/shell.toml` in sync with `generate_handler!`.
- Diagnostic: `browser_use_lifecycle_probe` (only when
  `ZCODE_SHELL_BROWSER_DEBUG=1`) verified the live round-trip: `listUserTabs`
  returned `{ok:true,userTabs:[]}` on the welcome screen, and
  `activateTab`/`close` with a missing tab returned `ref_not_found`.
  `claimTab`/`newTab`/`finalize`/`finalizeTabs`/`markDeliverable`/`markHandoff`/
  `nameSession`/`closeSession`/`recordingStart`/`recordingStatus`/
  `recordingCancel` route through the same bridge and return
  `backend_unavailable` (`no active browser workspace`) on the welcome screen;
  their controller behavior is covered by the renderer tests.
