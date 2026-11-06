/* eslint-disable max-lines -- Browser-use lifecycle commands, scope/claim registries and snapshot sync share one state machine; splitting them would create cross-module write paths. */
/**
 * Renderer-owned browser-use lifecycle commands.
 *
 * The agent's `BrowserCommand` surface includes tab lifecycle operations
 * (`activateTab`, `close`, `listUserTabs`) that are owned by the renderer: the
 * side-pane tab state machine is the single source of truth for which tabs
 * exist, which one is active, and which workspace/session owns them. This
 * module exposes those operations as a small controller so a host bridge (or a
 * future Tauri event endpoint) can call them without reaching into React state,
 * and publishes a snapshot for tab-state sync.
 *
 * Shell authority stays with `browser_view_*`: closing a tab calls
 * `platform.browserViewCloseTab`, and the existing shell events
 * (`onBrowserViewCloseTab`/`onBrowserViewReady`/…) continue to drive the same
 * state machine from the other direction.
 */
import {
  createUuid,
  type BrowserRecordingJob,
  type BrowserRecordingOptions,
  type BrowserViewportSize,
} from "@zcode/shared";
import type {
  BrowserUseSidePaneTab,
  WorkspaceSidePaneState,
} from "@/lib/workspaceSidePane.js";
import {
  closeSidePaneTab,
  openBrowserUseSidePane,
  setActiveSidePaneTab,
} from "@/lib/workspaceSidePane.js";
import type { BrowserUseRecordingPort } from "@/browser-use/browserUseRecordingPort.js";

export interface BrowserUseTabScope {
  workspaceKey: string;
  remoteSessionId?: string;
  /** `null` means the draft scope; `undefined` means "any session". */
  sessionId?: string | null;
}

export type BrowserUseTabLifecycle = "active" | "deliverable" | "handoff";

export interface BrowserUseTabSummary {
  tabId: string;
  url: string;
  title: string;
  /**
   * `{0,0}` means the renderer has not observed the guest viewport yet; it is an
   * explicit unknown sentinel, not a fabricated size.
   */
  viewport: BrowserViewportSize;
  active: boolean;
  /** Omitted while the tab is a normal active tab (matches the TS manager). */
  lifecycle?: Exclude<BrowserUseTabLifecycle, "active">;
}

export type BrowserUseLifecycleOutcome =
  | { ok: true; tab?: BrowserUseTabSummary; recording?: BrowserRecordingJob }
  | {
      ok: false;
      code:
        | "backend_unavailable"
        | "ref_not_found"
        | "invalid_input"
        | "capability_unsupported"
        | "execution_error";
      message: string;
    };

export interface BrowserUseFinalizeKeep {
  tabId: string;
  status: "handoff" | "deliverable";
}

export interface BrowserUseTabStateSnapshot {
  workspaceKey: string;
  sessionId: string | null;
  /** Set by `nameSession`; cleared by `closeSession`. */
  sessionName?: string;
  tabs: BrowserUseTabSummary[];
}

export interface BrowserUseCloseTabRequest {
  tabId: string;
  workspaceKey: string;
  remoteSessionId?: string;
  sessionId: string;
}

const UNKNOWN_VIEWPORT: BrowserViewportSize = { width: 0, height: 0 };

const urlByTabId = new Map<string, string>();
const urlListeners = new Set<() => void>();

/** Record the last URL reported by the browser pane. */
export function setBrowserUseTabUrl(tabId: string, url: string | null): void {
  if (url) urlByTabId.set(tabId, url);
  else urlByTabId.delete(tabId);
  for (const listener of urlListeners) listener();
}

export function getBrowserUseTabUrl(tabId: string): string | undefined {
  return urlByTabId.get(tabId);
}

export function subscribeBrowserUseTabUrl(listener: () => void): () => void {
  urlListeners.add(listener);
  return () => {
    urlListeners.delete(listener);
  };
}

const viewportByTabId = new Map<string, BrowserViewportSize>();
const viewportListeners = new Set<() => void>();

/** Record the guest viewport reported by `browserViewUpdateViewport`. */
export function setBrowserUseTabViewport(
  tabId: string,
  viewport: BrowserViewportSize | null,
): void {
  if (viewport) viewportByTabId.set(tabId, { ...viewport });
  else viewportByTabId.delete(tabId);
  for (const listener of viewportListeners) listener();
}

export function getBrowserUseTabViewport(tabId: string): BrowserViewportSize {
  const viewport = viewportByTabId.get(tabId);
  return viewport ? { ...viewport } : { ...UNKNOWN_VIEWPORT };
}

export function subscribeBrowserUseTabViewport(listener: () => void): () => void {
  viewportListeners.add(listener);
  return () => {
    viewportListeners.delete(listener);
  };
}

/**
 * Tab lifecycle owned by the manager: `active` is the default, `deliverable`
 * and `handoff` are final states set by `finalizeTabs`/`mark*`. The TS manager
 * keeps these on its `ManagedTab`; the renderer keeps the same information here
 * because the side-pane state is the source of truth.
 */
const lifecycleByTabId = new Map<string, BrowserUseTabLifecycle>();
/**
 * Tabs claimed by the agent. `claimTab` succeeds once per tab; a deliverable
 * finalized tab is released back to the user and becomes claimable again,
 * matching `releaseToUser` in the TS manager.
 */
const claimedTabIds = new Set<string>();

export function setBrowserUseTabLifecycle(
  tabId: string,
  lifecycle: BrowserUseTabLifecycle | null,
): void {
  if (lifecycle && lifecycle !== "active") lifecycleByTabId.set(tabId, lifecycle);
  else lifecycleByTabId.delete(tabId);
}

export function getBrowserUseTabLifecycle(tabId: string): BrowserUseTabLifecycle {
  return lifecycleByTabId.get(tabId) ?? "active";
}

export function isBrowserUseTabClaimed(tabId: string): boolean {
  return claimedTabIds.has(tabId);
}

/** Drop all renderer-side state for a tab that no longer exists. */
export function forgetBrowserUseTab(tabId: string): void {
  lifecycleByTabId.delete(tabId);
  claimedTabIds.delete(tabId);
}

const sessionNameByScope = new Map<string, string>();

/** Stable key for the exact workspace/remote/session scope. */
export function browserUseScopeKey(scope: BrowserUseTabScope): string {
  return [
    scope.workspaceKey,
    scope.remoteSessionId ?? "",
    scope.sessionId ?? "",
  ].join("\u0000");
}

export function getBrowserUseSessionName(scope: BrowserUseTabScope): string | undefined {
  return sessionNameByScope.get(browserUseScopeKey(scope));
}

export function setBrowserUseSessionName(
  scope: BrowserUseTabScope,
  name: string | null,
): void {
  if (name) sessionNameByScope.set(browserUseScopeKey(scope), name);
  else sessionNameByScope.delete(browserUseScopeKey(scope));
}

interface BrowserUseRecordingEntry {
  job: BrowserRecordingJob;
  tabId: string;
  port: BrowserUseRecordingPort;
  outputPath?: string;
  pending?: {
    data: Uint8Array;
    width: number;
    height: number;
    fps: number;
    durationMs: number;
    frameCount: number;
  };
  writing?: boolean;
}

const recordingsByScope = new Map<string, Map<string, BrowserUseRecordingEntry>>();

function recordingEntries(scope: BrowserUseTabScope): Map<string, BrowserUseRecordingEntry> {
  const key = browserUseScopeKey(scope);
  let entries = recordingsByScope.get(key);
  if (!entries) {
    entries = new Map();
    recordingsByScope.set(key, entries);
  }
  return entries;
}

function findRecordingEntry(
  scope: BrowserUseTabScope,
  recordingId: string,
): BrowserUseRecordingEntry | undefined {
  return recordingsByScope.get(browserUseScopeKey(scope))?.get(recordingId);
}

function activeRecordingForTab(
  scope: BrowserUseTabScope,
  tabId: string,
): BrowserUseRecordingEntry | undefined {
  return [...recordingEntries(scope).values()].find(
    (entry) => entry.tabId === tabId && entry.job.status === "running",
  );
}

function recordingSnapshot(entry: BrowserUseRecordingEntry): BrowserRecordingJob {
  return { ...entry.job };
}

function updateRecordingJob(
  entry: BrowserUseRecordingEntry,
  patch: Partial<BrowserRecordingJob>,
): void {
  entry.job = { ...entry.job, ...patch, updatedAt: Date.now() };
}

let currentSnapshot: BrowserUseTabStateSnapshot | null = null;
const snapshotListeners = new Set<(snapshot: BrowserUseTabStateSnapshot) => void>();

export function publishBrowserUseTabSnapshot(snapshot: BrowserUseTabStateSnapshot): void {
  currentSnapshot = snapshot;
  for (const listener of snapshotListeners) listener(snapshot);
}

export function getBrowserUseTabSnapshot(): BrowserUseTabStateSnapshot | null {
  return currentSnapshot;
}

export function subscribeBrowserUseTabSnapshot(
  listener: (snapshot: BrowserUseTabStateSnapshot) => void,
): () => void {
  snapshotListeners.add(listener);
  return () => {
    snapshotListeners.delete(listener);
  };
}

export function browserUseTabMatchesScope(
  tab: BrowserUseSidePaneTab,
  scope: BrowserUseTabScope,
): boolean {
  if (tab.type !== "browser-use") return false;
  if (tab.workspaceKey !== scope.workspaceKey) return false;
  if ((tab.remoteSessionId ?? "") !== (scope.remoteSessionId ?? "")) return false;
  if (scope.sessionId !== undefined && tab.sessionId !== scope.sessionId) return false;
  return true;
}

export function findBrowserUseTab(
  state: WorkspaceSidePaneState | null,
  tabId: string,
  scope: BrowserUseTabScope,
): BrowserUseSidePaneTab | null {
  if (!state) return null;
  return (
    state.tabs.find(
      (tab): tab is BrowserUseSidePaneTab =>
        tab.type === "browser-use" &&
        tab.tabId === tabId &&
        browserUseTabMatchesScope(tab, scope),
    ) ?? null
  );
}

/** Active browser-use tab in scope, falling back to the last tab (TS `resolveTab`). */
export function activeBrowserUseTab(
  state: WorkspaceSidePaneState | null,
  scope: BrowserUseTabScope,
): BrowserUseSidePaneTab | null {
  if (!state) return null;
  const tabs = state.tabs.filter(
    (tab): tab is BrowserUseSidePaneTab =>
      tab.type === "browser-use" && browserUseTabMatchesScope(tab, scope),
  );
  return tabs.find((tab) => tab.id === state.activeTabId) ?? tabs.at(-1) ?? null;
}

export function listBrowserUseTabs(
  state: WorkspaceSidePaneState | null,
  scope: BrowserUseTabScope,
  getUrl: (tabId: string) => string | undefined = getBrowserUseTabUrl,
  getViewport: (tabId: string) => BrowserViewportSize = getBrowserUseTabViewport,
): BrowserUseTabSummary[] {
  if (!state) return [];
  return state.tabs
    .filter(
      (tab): tab is BrowserUseSidePaneTab =>
        tab.type === "browser-use" && browserUseTabMatchesScope(tab, scope),
    )
    .map((tab) =>
      browserUseTabSummary(state, tab, getUrl, getViewport),
    );
}

function browserUseTabSummary(
  state: WorkspaceSidePaneState,
  tab: BrowserUseSidePaneTab,
  getUrl: (tabId: string) => string | undefined,
  getViewport: (tabId: string) => BrowserViewportSize,
): BrowserUseTabSummary {
  const lifecycle = getBrowserUseTabLifecycle(tab.tabId);
  return {
    tabId: tab.tabId,
    url: getUrl(tab.tabId) ?? "about:blank",
    title: tab.title ?? "",
    viewport: getViewport(tab.tabId),
    active: state.activeTabId === tab.id,
    ...(lifecycle === "active" ? {} : { lifecycle }),
  };
}

export function activateBrowserUseTab(
  state: WorkspaceSidePaneState | null,
  tabId: string,
  scope: BrowserUseTabScope,
): { state: WorkspaceSidePaneState | null; tab: BrowserUseSidePaneTab } | null {
  const tab = findBrowserUseTab(state, tabId, scope);
  if (!tab) return null;
  return { state: setActiveSidePaneTab(state, tab.id), tab };
}

export function closeBrowserUseTab(
  state: WorkspaceSidePaneState | null,
  tabId: string,
  scope: BrowserUseTabScope,
): { state: WorkspaceSidePaneState | null; tab: BrowserUseSidePaneTab } | null {
  const tab = findBrowserUseTab(state, tabId, scope);
  if (!tab) return null;
  return { state: closeSidePaneTab(state, tab.id), tab };
}

export function buildBrowserUseTabSnapshot(
  state: WorkspaceSidePaneState | null,
  scope: BrowserUseTabScope,
  getUrl: (tabId: string) => string | undefined = getBrowserUseTabUrl,
  getViewport: (tabId: string) => BrowserViewportSize = getBrowserUseTabViewport,
): BrowserUseTabStateSnapshot {
  const sessionName = getBrowserUseSessionName(scope);
  return {
    workspaceKey: scope.workspaceKey,
    sessionId: scope.sessionId ?? null,
    ...(sessionName ? { sessionName } : {}),
    tabs: listBrowserUseTabs(state, scope, getUrl, getViewport),
  };
}

export interface BrowserUseLifecycleHost {
  getState(): WorkspaceSidePaneState | null;
  commit(
    updater: (current: WorkspaceSidePaneState | null) => WorkspaceSidePaneState | null,
  ): WorkspaceSidePaneState | null;
  getUrl(tabId: string): string | undefined;
  getViewport?(tabId: string): BrowserViewportSize;
  reveal?(): void;
  closeShellTab?(payload: BrowserUseCloseTabRequest): Promise<void> | void;
  publish?(snapshot: BrowserUseTabStateSnapshot): void;
  /** Renderer capture port; absent = recording commands fail closed. */
  recording?: BrowserUseRecordingPort | null;
}

export interface BrowserUseLifecycleController {
  listUserTabs(): BrowserUseTabSummary[];
  activateTab(tabId: string): boolean;
  close(tabId: string): Promise<boolean>;
  /**
   * Claims a user tab: activates it, marks it agent-owned and returns its
   * summary. A tab can only be claimed once until `finalizeTabs` releases it
   * with `status: "deliverable"` (mirrors `claimTab`/`releaseToUser` in the TS
   * browser manager).
   */
  claimTab(tabId: string): BrowserUseLifecycleOutcome;
  /**
   * Creates a renderer-owned browser-use tab for the current session, activates
   * it and returns its summary. The child webview is created by the pane when
   * the tab mounts (Tauri) or by the guest manager (Electron).
   */
  newTab(): BrowserUseLifecycleOutcome;
  /**
   * Sets one tab's lifecycle (`deliverable` by default, `active` when
   * `deliverable: false`). It does not close the tab or release the claim.
   */
  finalize(options?: { tabId?: string; deliverable?: boolean }): BrowserUseLifecycleOutcome;
  /** Stores the browser session name for the current scope. */
  nameSession(name: string): BrowserUseLifecycleOutcome;
  /**
   * Releases all tabs in scope back to the user (clears lifecycle/claim state)
   * and clears the session name; tabs stay open, matching the TS manager.
   */
  closeSession(): BrowserUseLifecycleOutcome;
  /**
   * Marks the kept tabs as `handoff`/`deliverable`. Tabs outside `keep` are left
   * open and untouched (the TS manager also does not close them here).
   * Unknown keep ids fail the whole call.
   */
  finalizeTabs(keep: readonly BrowserUseFinalizeKeep[]): BrowserUseLifecycleOutcome;
  markDeliverable(tabId: string): BrowserUseLifecycleOutcome;
  markHandoff(tabId: string): BrowserUseLifecycleOutcome;
  /** Starts a real CDP-frame recording; fails closed without a capture port. */
  recordingStart(
    options?: BrowserRecordingOptions & { tabId?: string },
  ): BrowserUseLifecycleOutcome;
  /** Returns the job snapshot; `outputPath` selects the workspace target. */
  recordingStatus(
    recordingId: string,
    options?: { outputPath?: string; tabId?: string },
  ): BrowserUseLifecycleOutcome;
  /** Cancels a running recording and returns its cancelled snapshot. */
  recordingCancel(
    recordingId: string,
    options?: { tabId?: string },
  ): BrowserUseLifecycleOutcome;
  snapshot(): BrowserUseTabStateSnapshot;
}

export function createBrowserUseLifecycleController(
  host: BrowserUseLifecycleHost,
  scope: BrowserUseTabScope,
): BrowserUseLifecycleController {
  const getViewport = host.getViewport ?? getBrowserUseTabViewport;
  const publish = host.publish ?? publishBrowserUseTabSnapshot;
  const snapshot = () =>
    buildBrowserUseTabSnapshot(host.getState(), scope, host.getUrl, getViewport);
  const summarize = (tab: BrowserUseSidePaneTab): BrowserUseTabSummary | undefined => {
    const state = host.getState();
    return state ? browserUseTabSummary(state, tab, host.getUrl, getViewport) : undefined;
  };
  const missing = (tabId: string): BrowserUseLifecycleOutcome => ({
    ok: false,
    code: "backend_unavailable",
    message: `browser tab '${tabId}' is not visible in the current context`,
  });
  const markLifecycle = (
    tabId: string,
    lifecycle: Exclude<BrowserUseTabLifecycle, "active">,
  ): BrowserUseLifecycleOutcome => {
    const tab = findBrowserUseTab(host.getState(), tabId, scope);
    if (!tab) return missing(tabId);
    setBrowserUseTabLifecycle(tabId, lifecycle);
    publish(snapshot());
    const summary = summarize(tab);
    return { ok: true, ...(summary ? { tab: summary } : {}) };
  };
  const recordingPort = host.recording ?? null;
  const recordingError = (
    code: Extract<BrowserUseLifecycleOutcome, { ok: false }>["code"],
    message: string,
  ): BrowserUseLifecycleOutcome => ({ ok: false, code, message });
  const validateRecordingPath = (value: string): string | null => {
    const trimmed = value.trim();
    if (!trimmed) return "recording outputPath must not be empty";
    if (/^[/\\]/u.test(trimmed) || /^[A-Za-z]:[/\\]/u.test(trimmed)) {
      return "recording outputPath must be relative to the workspace";
    }
    const segments = trimmed.split(/[\\/]+/u);
    if (segments.some((segment) => segment === ".." || segment === "." || !segment)) {
      return "recording outputPath cannot escape the workspace";
    }
    if (!trimmed.toLowerCase().endsWith(".webm")) {
      return "recording outputPath must end with .webm";
    }
    return null;
  };
  const finalizeRecording = async (entry: BrowserUseRecordingEntry): Promise<void> => {
    if (!entry.pending || entry.writing) return;
    const pending = entry.pending;
    const relativePath =
      entry.outputPath ??
      `browser-recordings/${entry.job.id.replace(/[^A-Za-z0-9._-]/gu, "-")}.webm`;
    entry.writing = true;
    updateRecordingJob(entry, { phase: "finalizing", progress: 0.9 });
    try {
      if (!entry.port) throw new Error("browser recording port is unavailable");
      const path = await entry.port.write({
        jobId: entry.job.id,
        relativePath,
        data: pending.data,
      });
      entry.pending = undefined;
      updateRecordingJob(entry, {
        status: "completed",
        phase: "completed",
        progress: 1,
        artifact: {
          path,
          mimeType: "video/webm",
          width: pending.width,
          height: pending.height,
          fps: pending.fps,
          durationMs: pending.durationMs,
          frameCount: pending.frameCount,
        },
      });
    } catch (error) {
      entry.pending = undefined;
      updateRecordingJob(entry, {
        status: "failed",
        phase: "failed",
        error: error instanceof Error ? error.message : String(error),
      });
    } finally {
      entry.writing = false;
    }
  };
  const cancelRecordingsForTab = (tabId: string): void => {
    for (const entry of recordingEntries(scope).values()) {
      if (entry.tabId !== tabId || entry.job.status !== "running") continue;
      entry.port.cancel(entry.job.id);
      updateRecordingJob(entry, { status: "cancelled", phase: "cancelled" });
    }
  };
  return {
    listUserTabs: () => snapshot().tabs,
    activateTab(tabId) {
      const result = activateBrowserUseTab(host.getState(), tabId, scope);
      if (!result) return false;
      host.commit(() => result.state);
      host.reveal?.();
      publish(snapshot());
      return true;
    },
    async close(tabId) {
      const result = closeBrowserUseTab(host.getState(), tabId, scope);
      if (!result) return false;
      host.commit(() => result.state);
      cancelRecordingsForTab(result.tab.tabId);
      forgetBrowserUseTab(result.tab.tabId);
      await host.closeShellTab?.({
        tabId: result.tab.tabId,
        workspaceKey: result.tab.workspaceKey ?? scope.workspaceKey,
        ...(result.tab.remoteSessionId
          ? { remoteSessionId: result.tab.remoteSessionId }
          : {}),
        sessionId: result.tab.sessionId,
      });
      publish(snapshot());
      return true;
    },
    claimTab(tabId) {
      const tab = findBrowserUseTab(host.getState(), tabId, scope);
      if (!tab) return missing(tabId);
      if (claimedTabIds.has(tabId)) {
        return {
          ok: false,
          code: "backend_unavailable",
          message: `user browser tab '${tabId}' is unavailable or already claimed`,
        };
      }
      claimedTabIds.add(tabId);
      setBrowserUseTabLifecycle(tabId, "active");
      const result = activateBrowserUseTab(host.getState(), tabId, scope);
      if (result) host.commit(() => result.state);
      host.reveal?.();
      publish(snapshot());
      const summary = summarize(tab);
      return { ok: true, ...(summary ? { tab: summary } : {}) };
    },
    newTab() {
      const sessionId = scope.sessionId;
      if (!sessionId) {
        return {
          ok: false,
          code: "backend_unavailable",
          message: "no active browser session",
        };
      }
      const tabId = `browser-${createUuid()}`;
      const next = openBrowserUseSidePane(host.getState(), {
        workspaceKey: scope.workspaceKey,
        sessionId,
        tabId,
        ...(scope.remoteSessionId ? { remoteSessionId: scope.remoteSessionId } : {}),
        activate: true,
      });
      host.commit(() => next);
      claimedTabIds.add(tabId);
      setBrowserUseTabLifecycle(tabId, "active");
      host.reveal?.();
      publish(snapshot());
      const tab = findBrowserUseTab(next, tabId, scope);
      const summary = tab
        ? browserUseTabSummary(next, tab, host.getUrl, getViewport)
        : undefined;
      return { ok: true, ...(summary ? { tab: summary } : {}) };
    },
    finalize(options = {}) {
      const state = host.getState();
      const tab = options.tabId
        ? findBrowserUseTab(state, options.tabId, scope)
        : activeBrowserUseTab(state, scope);
      if (!tab) return missing(options.tabId ?? "(active)");
      const lifecycle = options.deliverable === false ? "active" : "deliverable";
      setBrowserUseTabLifecycle(tab.tabId, lifecycle);
      publish(snapshot());
      const summary = state
        ? browserUseTabSummary(state, tab, host.getUrl, getViewport)
        : undefined;
      return { ok: true, ...(summary ? { tab: summary } : {}) };
    },
    nameSession(name) {
      const trimmed = name.trim();
      if (!trimmed) {
        return {
          ok: false,
          code: "invalid_input",
          message: "nameSession requires a non-empty name",
        };
      }
      setBrowserUseSessionName(scope, trimmed);
      publish(snapshot());
      return { ok: true };
    },
    closeSession() {
      for (const entry of recordingEntries(scope).values()) {
        if (entry.job.status !== "running") continue;
        entry.port.cancel(entry.job.id);
        updateRecordingJob(entry, { status: "cancelled", phase: "cancelled" });
      }
      for (const tab of listBrowserUseTabs(
        host.getState(),
        scope,
        host.getUrl,
        getViewport,
      )) {
        forgetBrowserUseTab(tab.tabId);
      }
      setBrowserUseSessionName(scope, null);
      publish(snapshot());
      return { ok: true };
    },
    finalizeTabs(keep) {
      const tabs = listBrowserUseTabs(host.getState(), scope, host.getUrl, getViewport);
      const owned = new Set(tabs.map((tab) => tab.tabId));
      const unknown = keep
        .map((item) => item.tabId)
        .filter((tabId) => !owned.has(tabId));
      if (unknown.length > 0) {
        return {
          ok: false,
          code: "backend_unavailable",
          message: `cannot finalize unknown tab(s): ${unknown.join(", ")}`,
        };
      }
      for (const item of keep) {
        setBrowserUseTabLifecycle(item.tabId, item.status);
        // `deliverable` releases the tab back to the user (TS `releaseToUser`),
        // so it becomes claimable again; `handoff` stays agent-owned.
        if (item.status === "deliverable") claimedTabIds.delete(item.tabId);
      }
      publish(snapshot());
      return { ok: true };
    },
    markDeliverable: (tabId) => markLifecycle(tabId, "deliverable"),
    markHandoff: (tabId) => markLifecycle(tabId, "handoff"),
    recordingStart(options = {}) {
      if (!recordingPort) {
        return recordingError(
          "capability_unsupported",
          "browser recording is unavailable: no capture/encoder port is attached",
        );
      }
      const tab = options.tabId
        ? findBrowserUseTab(host.getState(), options.tabId, scope)
        : activeBrowserUseTab(host.getState(), scope);
      if (!tab) return missing(options.tabId ?? "(active)");
      const active = activeRecordingForTab(scope, tab.tabId);
      if (active) {
        return recordingError(
          "execution_error",
          `browser tab '${tab.tabId}' already has active recording '${active.job.id}'`,
        );
      }
      const now = Date.now();
      const entry: BrowserUseRecordingEntry = {
        tabId: tab.tabId,
        port: recordingPort,
        job: {
          id: `iab-recording:${createUuid()}`,
          status: "running",
          phase: "preparing",
          progress: 0,
          startedAt: now,
          updatedAt: now,
        },
      };
      recordingEntries(scope).set(entry.job.id, entry);
      recordingPort.start({
        jobId: entry.job.id,
        tabId: tab.tabId,
        options,
        onPhase: (phase, progress) => {
          if (entry.job.status !== "running") return;
          updateRecordingJob(entry, { phase, progress });
        },
        onComplete: (result) => {
          if (entry.job.status !== "running") return;
          entry.pending = result;
          void finalizeRecording(entry);
        },
        onError: (error) => {
          if (entry.job.status !== "running") return;
          updateRecordingJob(entry, {
            status: "failed",
            phase: "failed",
            error,
          });
        },
      });
      return { ok: true, recording: recordingSnapshot(entry) };
    },
    recordingStatus(recordingId, options = {}) {
      const entry = findRecordingEntry(scope, recordingId);
      if (!entry) {
        return recordingError(
          "backend_unavailable",
          `browser recording '${recordingId}' is unavailable in this context`,
        );
      }
      if (options.tabId && options.tabId !== entry.tabId) {
        return recordingError(
          "backend_unavailable",
          `browser recording '${recordingId}' does not belong to tab '${options.tabId}'`,
        );
      }
      if (options.outputPath !== undefined) {
        const error = validateRecordingPath(options.outputPath);
        if (error) return recordingError("invalid_input", error);
        entry.outputPath = options.outputPath.trim();
      }
      if (entry.pending && !entry.writing) void finalizeRecording(entry);
      return { ok: true, recording: recordingSnapshot(entry) };
    },
    recordingCancel(recordingId, options = {}) {
      const entry = findRecordingEntry(scope, recordingId);
      if (!entry) {
        return recordingError(
          "backend_unavailable",
          `browser recording '${recordingId}' is unavailable in this context`,
        );
      }
      if (options.tabId && options.tabId !== entry.tabId) {
        return recordingError(
          "backend_unavailable",
          `browser recording '${recordingId}' does not belong to tab '${options.tabId}'`,
        );
      }
      if (entry.job.status === "running") {
        entry.port.cancel(entry.job.id);
        entry.pending = undefined;
        updateRecordingJob(entry, { status: "cancelled", phase: "cancelled" });
      }
      return { ok: true, recording: recordingSnapshot(entry) };
    },
    snapshot,
  };
}

let activeController: BrowserUseLifecycleController | null = null;

/** Register the live controller for a host bridge (last registration wins). */
export function registerBrowserUseLifecycleController(
  controller: BrowserUseLifecycleController,
): () => void {
  activeController = controller;
  return () => {
    if (activeController === controller) activeController = null;
  };
}

export function getBrowserUseLifecycleController(): BrowserUseLifecycleController | null {
  return activeController;
}
