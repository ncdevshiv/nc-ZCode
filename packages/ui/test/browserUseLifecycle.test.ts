/**
 * Renderer-owned browser-use lifecycle commands.
 *
 * Run from `packages/ui`:
 *   node --import tsx --test test/browserUseLifecycle.test.ts
 */
import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import type { WorkspaceSidePaneState } from "../src/lib/workspaceSidePane.js";
import type { BrowserUseRecordingPort } from "../src/browser-use/browserUseRecordingPort.js";
import {
  CURSOR_OVERLAY_ID,
  cursorCleanupScript,
  cursorOverlayScript,
  executeRecordingActions,
  fillSelectorScript,
  selectorPointScript,
  selectorScrollTargetScript,
  waitForSelectorScript,
} from "../src/browser-use/browserUseRecordingActions.js";
import {
  activateBrowserUseTab,
  buildBrowserUseTabSnapshot,
  closeBrowserUseTab,
  createBrowserUseLifecycleController,
  getBrowserUseLifecycleController,
  getBrowserUseTabSnapshot,
  getBrowserUseTabUrl,
  getBrowserUseTabViewport,
  listBrowserUseTabs,
  publishBrowserUseTabSnapshot,
  registerBrowserUseLifecycleController,
  setBrowserUseTabUrl,
  setBrowserUseTabViewport,
  subscribeBrowserUseTabSnapshot,
  type BrowserUseCloseTabRequest,
  type BrowserUseTabScope,
} from "../src/browser-use/browserUseLifecycle.js";

const scope: BrowserUseTabScope = {
  workspaceKey: "workspace-a",
  sessionId: "session-1",
};

function browserState(
  tabIds: string[],
  options: { activeIndex?: number; sessionId?: string; workspaceKey?: string } = {},
): WorkspaceSidePaneState {
  const { activeIndex = 0, sessionId = "session-1", workspaceKey = "workspace-a" } = options;
  return {
    activeTabId: `browser-use:${tabIds[activeIndex] ?? ""}`,
    tabs: tabIds.map((tabId) => ({
      id: `browser-use:${tabId}`,
      type: "browser-use",
      workspaceKey,
      sessionId,
      tabId,
      title: tabId,
    })),
  } as unknown as WorkspaceSidePaneState;
}

function state(): WorkspaceSidePaneState {
  return {
    activeTabId: "browser-use:tab-1",
    tabs: [
      {
        id: "browser-use:tab-1",
        type: "browser-use",
        workspaceKey: "workspace-a",
        sessionId: "session-1",
        tabId: "tab-1",
        title: "One",
      },
      {
        id: "browser-use:tab-2",
        type: "browser-use",
        workspaceKey: "workspace-a",
        sessionId: "session-1",
        tabId: "tab-2",
        title: "Two",
      },
      {
        id: "browser-use:tab-3",
        type: "browser-use",
        workspaceKey: "workspace-a",
        sessionId: "session-2",
        tabId: "tab-3",
        title: "Other session",
      },
      {
        id: "browser-use:tab-4",
        type: "browser-use",
        workspaceKey: "workspace-b",
        sessionId: "session-1",
        tabId: "tab-4",
        title: "Other workspace",
      },
      { id: "git", type: "git" },
    ],
  } as unknown as WorkspaceSidePaneState;
}

test("lists only browser-use tabs in the current workspace/session scope", () => {
  setBrowserUseTabUrl("tab-1", "https://example.com/a");
  setBrowserUseTabUrl("tab-2", "https://example.com/b");
  setBrowserUseTabViewport("tab-1", { width: 1280, height: 720 });

  const tabs = listBrowserUseTabs(state(), scope);
  assert.deepEqual(
    tabs.map((tab) => tab.tabId),
    ["tab-1", "tab-2"],
  );
  assert.equal(tabs[0]?.url, "https://example.com/a");
  assert.equal(tabs[0]?.title, "One");
  assert.deepEqual(tabs[0]?.viewport, { width: 1280, height: 720 });
  assert.equal(tabs[0]?.active, true);
  assert.equal(tabs[1]?.active, false);

  setBrowserUseTabUrl("tab-2", null);
  assert.equal(getBrowserUseTabUrl("tab-2"), undefined);
  assert.equal(listBrowserUseTabs(state(), scope)[1]?.url, "about:blank");
});

test("activateTab changes the active tab without touching other tabs", () => {
  const result = activateBrowserUseTab(state(), "tab-2", scope);
  assert.ok(result);
  assert.equal(result.tab.tabId, "tab-2");
  assert.equal(result.state?.activeTabId, "browser-use:tab-2");
  assert.equal(result.state?.tabs.length, 5);
});

test("cross-workspace and cross-session tabs are not addressable", () => {
  assert.equal(activateBrowserUseTab(state(), "tab-3", scope), null);
  assert.equal(activateBrowserUseTab(state(), "tab-4", scope), null);
  assert.equal(closeBrowserUseTab(state(), "tab-3", scope), null);
  assert.equal(closeBrowserUseTab(state(), "tab-4", scope), null);
  assert.equal(activateBrowserUseTab(state(), "missing", scope), null);
});

test("close removes the tab and falls back to the next active tab", () => {
  const result = closeBrowserUseTab(state(), "tab-1", scope);
  assert.ok(result);
  assert.deepEqual(
    result.state?.tabs.map((tab) => tab.id),
    ["browser-use:tab-2", "browser-use:tab-3", "browser-use:tab-4", "git"],
  );
  assert.equal(result.state?.activeTabId, "browser-use:tab-2");
});

test("controller calls reveal/closeShellTab and publishes a snapshot", async () => {
  let current = state();
  const reveals: number[] = [];
  const closed: BrowserUseCloseTabRequest[] = [];
  const published: string[][] = [];
  const controller = createBrowserUseLifecycleController(
    {
      getState: () => current,
      commit: (updater) => {
        current = updater(current) ?? current;
        return current;
      },
      getUrl: (tabId) => (tabId === "tab-1" ? "https://example.com" : undefined),
      reveal: () => reveals.push(1),
      closeShellTab: (payload) => {
        closed.push(payload);
      },
      publish: (snapshot) => published.push(snapshot.tabs.map((tab) => tab.tabId)),
    },
    scope,
  );

  assert.equal(controller.activateTab("tab-2"), true);
  assert.equal(reveals.length, 1);
  assert.equal(current.activeTabId, "browser-use:tab-2");
  assert.deepEqual(published.at(-1), ["tab-1", "tab-2"]);

  assert.equal(await controller.close("tab-1"), true);
  assert.deepEqual(closed, [
    { tabId: "tab-1", workspaceKey: "workspace-a", sessionId: "session-1" },
  ]);
  assert.deepEqual(
    current.tabs.map((tab) => tab.id),
    ["browser-use:tab-2", "browser-use:tab-3", "browser-use:tab-4", "git"],
  );

  assert.equal(controller.activateTab("missing"), false);
  assert.equal(await controller.close("missing"), false);
  assert.equal(reveals.length, 1);
  assert.equal(closed.length, 1);
});

test("snapshot publish/subscribe delivers and unsubscribes", () => {
  const received: string[][] = [];
  const dispose = subscribeBrowserUseTabSnapshot((snapshot) => {
    received.push(snapshot.tabs.map((tab) => tab.tabId));
  });
  publishBrowserUseTabSnapshot(buildBrowserUseTabSnapshot(state(), scope));
  assert.deepEqual(getBrowserUseTabSnapshot()?.tabs.map((tab) => tab.tabId), [
    "tab-1",
    "tab-2",
  ]);
  dispose();
  publishBrowserUseTabSnapshot({
    workspaceKey: "workspace-a",
    sessionId: "session-1",
    tabs: [],
  });
  assert.deepEqual(received, [["tab-1", "tab-2"]]);
});

test("viewport defaults to the explicit unknown sentinel and can be cleared", () => {
  setBrowserUseTabViewport("tab-x", { width: 800, height: 600 });
  assert.deepEqual(getBrowserUseTabViewport("tab-x"), { width: 800, height: 600 });
  setBrowserUseTabViewport("tab-x", null);
  assert.deepEqual(getBrowserUseTabViewport("tab-x"), { width: 0, height: 0 });
});

test("controller registry keeps the latest registration", () => {
  const first = createBrowserUseLifecycleController(
    {
      getState: () => null,
      commit: () => null,
      getUrl: () => undefined,
    },
    scope,
  );
  const second = createBrowserUseLifecycleController(
    {
      getState: () => null,
      commit: () => null,
      getUrl: () => undefined,
    },
    scope,
  );
  const disposeFirst = registerBrowserUseLifecycleController(first);
  assert.equal(getBrowserUseLifecycleController(), first);
  const disposeSecond = registerBrowserUseLifecycleController(second);
  assert.equal(getBrowserUseLifecycleController(), second);
  disposeSecond();
  assert.equal(getBrowserUseLifecycleController(), null);
  disposeFirst();
  assert.equal(getBrowserUseLifecycleController(), null);
});

function controllerFor(
  initial: WorkspaceSidePaneState | null,
  controllerScope: BrowserUseTabScope = scope,
  recording?: BrowserUseRecordingPort | null,
) {
  let current: WorkspaceSidePaneState | null = initial;
  const controller = createBrowserUseLifecycleController(
    {
      getState: () => current,
      commit: (updater) => {
        current = updater(current);
        return current;
      },
      getUrl: () => undefined,
      publish: () => undefined,
      recording,
    },
    controllerScope,
  );
  return {
    controller,
    getState: () => current,
  };
}

function createRecordingPort(
  onStart?: (input: Parameters<BrowserUseRecordingPort["start"]>[0]) => void,
): BrowserUseRecordingPort {
  return {
    start(input) {
      onStart?.(input);
    },
    cancel() {},
    async write({ relativePath, data }) {
      const root = await mkdtemp(join(tmpdir(), "zcode-recording-test-"));
      const path = join(root, relativePath.replace(/[\\/]/gu, "_"));
      await writeFile(path, data);
      return path;
    },
  };
}

test("claimTab claims once, activates the tab and rejects a second claim", () => {
  const { controller, getState } = controllerFor(
    browserState(["claim-tab-a", "claim-tab-b"]),
  );
  const claimed = controller.claimTab("claim-tab-a");
  assert.equal(claimed.ok, true);
  if (claimed.ok) {
    assert.equal(claimed.tab?.tabId, "claim-tab-a");
    assert.equal(claimed.tab?.active, true);
    assert.equal(claimed.tab?.lifecycle, undefined);
  }
  assert.equal(getState().activeTabId, "browser-use:claim-tab-a");

  const second = controller.claimTab("claim-tab-a");
  assert.equal(second.ok, false);
  if (!second.ok) {
    assert.equal(second.code, "backend_unavailable");
    assert.match(second.message, /already claimed/);
  }

  const missing = controller.claimTab("claim-tab-missing");
  assert.equal(missing.ok, false);
  if (!missing.ok) assert.equal(missing.code, "backend_unavailable");
});

test("markDeliverable/markHandoff update the summary lifecycle", () => {
  const { controller } = controllerFor(browserState(["mark-tab-a"]));

  const deliverable = controller.markDeliverable("mark-tab-a");
  assert.equal(deliverable.ok, true);
  if (deliverable.ok) assert.equal(deliverable.tab?.lifecycle, "deliverable");
  assert.equal(controller.listUserTabs()[0]?.lifecycle, "deliverable");

  const handoff = controller.markHandoff("mark-tab-a");
  assert.equal(handoff.ok, true);
  if (handoff.ok) assert.equal(handoff.tab?.lifecycle, "handoff");
  assert.equal(controller.listUserTabs()[0]?.lifecycle, "handoff");

  const missing = controller.markHandoff("mark-tab-missing");
  assert.equal(missing.ok, false);
  if (!missing.ok) assert.equal(missing.code, "backend_unavailable");
});

test("finalizeTabs marks kept tabs and leaves other tabs open", () => {
  const { controller, getState } = controllerFor(
    browserState(["final-keep", "final-handoff", "final-untouched"]),
  );
  const outcome = controller.finalizeTabs([
    { tabId: "final-keep", status: "deliverable" },
    { tabId: "final-handoff", status: "handoff" },
  ]);
  assert.equal(outcome.ok, true);
  assert.deepEqual(
    getState().tabs.map((tab) => tab.id),
    ["browser-use:final-keep", "browser-use:final-handoff", "browser-use:final-untouched"],
  );
  const summaries = controller.listUserTabs();
  assert.equal(summaries.find((tab) => tab.tabId === "final-keep")?.lifecycle, "deliverable");
  assert.equal(summaries.find((tab) => tab.tabId === "final-handoff")?.lifecycle, "handoff");
  assert.equal(summaries.find((tab) => tab.tabId === "final-untouched")?.lifecycle, undefined);
});

test("finalizeTabs rejects unknown keep ids without mutating lifecycle", () => {
  const { controller } = controllerFor(browserState(["final-unknown-a"]));
  const outcome = controller.finalizeTabs([
    { tabId: "final-unknown-a", status: "handoff" },
    { tabId: "final-unknown-missing", status: "deliverable" },
  ]);
  assert.equal(outcome.ok, false);
  if (!outcome.ok) {
    assert.equal(outcome.code, "backend_unavailable");
    assert.match(outcome.message, /cannot finalize unknown tab\(s\): final-unknown-missing/);
  }
  assert.equal(controller.listUserTabs()[0]?.lifecycle, undefined);
});

test("deliverable releases the claim while handoff keeps it", () => {
  const { controller } = controllerFor(browserState(["release-tab-a", "handoff-tab-a"]));

  assert.equal(controller.claimTab("release-tab-a").ok, true);
  assert.equal(controller.claimTab("release-tab-a").ok, false);
  assert.equal(
    controller.finalizeTabs([{ tabId: "release-tab-a", status: "deliverable" }]).ok,
    true,
  );
  assert.equal(controller.claimTab("release-tab-a").ok, true);

  assert.equal(controller.claimTab("handoff-tab-a").ok, true);
  assert.equal(
    controller.finalizeTabs([{ tabId: "handoff-tab-a", status: "handoff" }]).ok,
    true,
  );
  assert.equal(controller.claimTab("handoff-tab-a").ok, false);
});

test("closing a tab clears its lifecycle and claim state", async () => {
  const { controller, getState } = controllerFor(browserState(["close-tab-a"]));
  assert.equal(controller.claimTab("close-tab-a").ok, true);
  assert.equal(controller.markDeliverable("close-tab-a").ok, true);
  assert.equal(await controller.close("close-tab-a"), true);
  assert.equal(getState()?.tabs.length ?? 0, 0);

  // Re-registering the same tab id (e.g. renderer restore) starts unclaimed.
  const restored = controllerFor(browserState(["close-tab-a"]));
  assert.equal(restored.controller.claimTab("close-tab-a").ok, true);
  assert.equal(restored.controller.listUserTabs()[0]?.lifecycle, undefined);
});

test("newTab creates an active, already-claimed tab in scope", () => {
  const { controller, getState } = controllerFor(browserState(["existing-tab"]));
  const outcome = controller.newTab();
  assert.equal(outcome.ok, true);
  const tabId = outcome.ok ? outcome.tab?.tabId : undefined;
  assert.ok(tabId);
  assert.equal(outcome.ok ? outcome.tab?.active : false, true);
  assert.equal(getState()?.tabs.length, 2);
  assert.equal(getState()?.activeTabId, `browser-use:${tabId}`);
  assert.equal(controller.listUserTabs().some((tab) => tab.tabId === tabId), true);
  const claim = controller.claimTab(tabId);
  assert.equal(claim.ok, false);
  if (!claim.ok) assert.match(claim.message, /already claimed/);
});

test("single-tab finalize defaults to deliverable and can reset to active", () => {
  const { controller } = controllerFor(browserState(["fin-a", "fin-b"], { activeIndex: 1 }));
  const outcome = controller.finalize();
  assert.equal(outcome.ok, true);
  if (outcome.ok) {
    assert.equal(outcome.tab?.tabId, "fin-b");
    assert.equal(outcome.tab?.lifecycle, "deliverable");
  }
  const reset = controller.finalize({ tabId: "fin-b", deliverable: false });
  assert.equal(reset.ok, true);
  if (reset.ok) assert.equal(reset.tab?.lifecycle, undefined);
  const missing = controller.finalize({ tabId: "fin-missing" });
  assert.equal(missing.ok, false);
  if (!missing.ok) assert.equal(missing.code, "backend_unavailable");
});

test("nameSession stores the name and closeSession releases tabs and the name", () => {
  const nameScope: BrowserUseTabScope = {
    workspaceKey: "workspace-name",
    sessionId: "session-name",
  };
  const { controller } = controllerFor(
    browserState(["name-tab"], {
      workspaceKey: "workspace-name",
      sessionId: "session-name",
    }),
    nameScope,
  );
  assert.equal(controller.claimTab("name-tab").ok, true);
  assert.equal(controller.markHandoff("name-tab").ok, true);
  assert.equal(controller.nameSession("  Checkout flow  ").ok, true);
  assert.equal(controller.snapshot().sessionName, "Checkout flow");

  const empty = controller.nameSession("   ");
  assert.equal(empty.ok, false);
  if (!empty.ok) assert.equal(empty.code, "invalid_input");

  const closed = controller.closeSession();
  assert.equal(closed.ok, true);
  assert.equal(controller.snapshot().sessionName, undefined);
  const summary = controller.listUserTabs()[0];
  assert.equal(summary?.tabId, "name-tab");
  assert.equal(summary?.lifecycle, undefined);
  assert.equal(controller.claimTab("name-tab").ok, true);
});

test("recording fails closed without a capture port", () => {
  const { controller } = controllerFor(browserState(["rec-none"]));
  const start = controller.recordingStart({ tabId: "rec-none" });
  assert.equal(start.ok, false);
  if (!start.ok) {
    assert.equal(start.code, "capability_unsupported");
    assert.match(start.message, /capture\/encoder port/);
  }
});

test("recording passes scripted actions to the capture port", () => {
  let receivedActions: unknown;
  const port = createRecordingPort((input) => {
    receivedActions = input.options.actions;
  });
  const { controller } = controllerFor(browserState(["rec-actions"]), scope, port);
  const start = controller.recordingStart({
    tabId: "rec-actions",
    actions: [{ type: "wait", durationMs: 10 }],
  });
  assert.equal(start.ok, true);
  assert.deepEqual(receivedActions, [{ type: "wait", durationMs: 10 }]);
});

test("recording action scripts escape selectors/text and keep the cursor contract", () => {
  const overlay = cursorOverlayScript();
  assert.ok(overlay.includes(CURSOR_OVERLAY_ID));
  assert.ok(overlay.includes("mousemove"));
  assert.ok(overlay.includes("mousedown"));
  assert.ok(overlay.includes("__zcodeBrowserRecordingCursorCleanup"));
  assert.equal(
    cursorCleanupScript(),
    "window.__zcodeBrowserRecordingCursorCleanup?.()",
  );

  const selector = 'a[href="x"]';
  assert.ok(selectorPointScript(selector).includes(JSON.stringify(selector)));
  assert.ok(selectorScrollTargetScript(selector).includes("window.scrollX"));

  const text = 'hello "world"\\n';
  const fill = fillSelectorScript(selector, text);
  assert.ok(fill.includes(JSON.stringify(selector)));
  assert.ok(fill.includes(JSON.stringify(text)));

  const wait = waitForSelectorScript("#missing", "detached");
  assert.ok(wait.includes(JSON.stringify("#missing")));
  assert.ok(wait.includes('"detached"'));
});

test("recordingStart/status writes a completed WebM artifact", async () => {
  const webm = new Uint8Array([0x1a, 0x45, 0xdf, 0xa3, 0x01, 0x02, 0x03]);
  const port = createRecordingPort((input) => {
    input.onPhase("capturing", 0.1);
    input.onComplete({
      data: webm,
      width: 800,
      height: 600,
      fps: 25,
      durationMs: 500,
      frameCount: 12,
    });
  });
  const { controller } = controllerFor(browserState(["rec-tab"]), scope, port);
  const start = controller.recordingStart({ tabId: "rec-tab" });
  assert.equal(start.ok, true);
  const recordingId = start.ok ? start.recording?.id : undefined;
  assert.ok(recordingId);
  let status = controller.recordingStatus(recordingId, {
    outputPath: "recordings/demo.webm",
  });
  for (let attempt = 0; attempt < 50; attempt += 1) {
    if (status.ok && status.recording?.status === "completed") break;
    await new Promise((resolve) => setTimeout(resolve, 10));
    status = controller.recordingStatus(recordingId, {
      outputPath: "recordings/demo.webm",
    });
  }
  assert.equal(status.ok, true);
  if (!status.ok) return;
  assert.equal(status.recording?.status, "completed");
  assert.equal(status.recording?.phase, "completed");
  assert.equal(status.recording?.progress, 1);
  assert.equal(status.recording?.artifact?.mimeType, "video/webm");
  assert.equal(status.recording?.artifact?.frameCount, 12);
  const path = status.recording?.artifact?.path;
  assert.ok(path);
  const bytes = await readFile(path);
  assert.deepEqual([...bytes], [...webm]);
  await rm(dirname(path), { recursive: true, force: true });
});

test("recordingStart rejects a second active recording for the same tab", () => {
  const { controller } = controllerFor(
    browserState(["rec-dup"]),
    scope,
    createRecordingPort(),
  );
  const first = controller.recordingStart({ tabId: "rec-dup" });
  assert.equal(first.ok, true);
  const second = controller.recordingStart({ tabId: "rec-dup" });
  assert.equal(second.ok, false);
  if (!second.ok) {
    assert.equal(second.code, "execution_error");
    assert.match(second.message, /already has active recording/);
  }
});

test("recordingCancel marks a running job cancelled and calls the port", () => {
  let cancelCalls = 0;
  const base = createRecordingPort();
  const port: BrowserUseRecordingPort = {
    ...base,
    cancel: () => {
      cancelCalls += 1;
    },
  };
  const { controller } = controllerFor(browserState(["rec-cancel"]), scope, port);
  const start = controller.recordingStart({ tabId: "rec-cancel" });
  assert.equal(start.ok, true);
  const recordingId = start.ok ? start.recording?.id : undefined;
  assert.ok(recordingId);
  const cancelled = controller.recordingCancel(recordingId);
  assert.equal(cancelled.ok, true);
  if (cancelled.ok) {
    assert.equal(cancelled.recording?.status, "cancelled");
    assert.equal(cancelled.recording?.phase, "cancelled");
  }
  assert.equal(cancelCalls, 1);
});

test("recordingStatus rejects unknown ids and unsafe output paths", () => {
  const { controller } = controllerFor(
    browserState(["rec-validate"]),
    scope,
    createRecordingPort(),
  );
  const unknown = controller.recordingStatus("missing-recording");
  assert.equal(unknown.ok, false);
  if (!unknown.ok) assert.equal(unknown.code, "backend_unavailable");

  const start = controller.recordingStart({ tabId: "rec-validate" });
  assert.equal(start.ok, true);
  const recordingId = start.ok ? start.recording?.id : undefined;
  assert.ok(recordingId);
  for (const outputPath of ["../escape.webm", "/abs/escape.webm", "recording.mp4"]) {
    const result = controller.recordingStatus(recordingId, { outputPath });
    assert.equal(result.ok, false);
    if (!result.ok) assert.equal(result.code, "invalid_input");
  }
});

test("closeSession cancels running recordings in scope", () => {
  let cancelCalls = 0;
  const base = createRecordingPort();
  const port: BrowserUseRecordingPort = {
    ...base,
    cancel: () => {
      cancelCalls += 1;
    },
  };
  const { controller } = controllerFor(browserState(["rec-close"]), scope, port);
  const start = controller.recordingStart({ tabId: "rec-close" });
  assert.equal(start.ok, true);
  const recordingId = start.ok ? start.recording?.id : undefined;
  assert.ok(recordingId);
  assert.equal(controller.closeSession().ok, true);
  const status = controller.recordingStatus(recordingId);
  assert.equal(status.ok, true);
  if (status.ok) assert.equal(status.recording?.status, "cancelled");
  assert.equal(cancelCalls, 1);
});

test("executeRecordingActions drives mouse, fill, scroll and waitFor over CDP", async () => {
  const calls: Array<{ method: string; params?: Record<string, unknown> }> = [];
  const cdp = async (method: string, params?: Record<string, unknown>) => {
    calls.push({ method, params });
    if (method !== "Runtime.evaluate") return {};
    const expression = String(params?.expression ?? "");
    if (expression.includes("attached")) {
      return { result: { value: { attached: true, visible: true } } };
    }
    if (expression.includes("window.scrollX") && expression.includes("getBoundingClientRect")) {
      return { result: { value: { x: 0, y: 240 } } };
    }
    if (expression.includes("getBoundingClientRect")) {
      return { result: { value: { x: 120, y: 80 } } };
    }
    if (expression.includes("window.scrollX")) {
      return { result: { value: { x: 0, y: 0 } } };
    }
    return { result: { value: true } };
  };

  await executeRecordingActions(
    [
      { type: "move", x: 10, y: 20 },
      { type: "click", selector: "#go" },
      { type: "type", selector: "#name", text: "hello" },
      { type: "scrollTo", y: 240 },
      { type: "waitFor", selector: "#ready" },
      { type: "drag", path: [{ x: 1, y: 1 }, { x: 5, y: 5 }] },
    ],
    {
      cdp,
      isCancelled: () => false,
      sleep: async () => {},
      viewport: { width: 800, height: 600 },
    },
  );

  const mouse = calls.filter((call) => call.method === "Input.dispatchMouseEvent");
  assert.ok(mouse.some((call) => call.params?.type === "mouseMoved" && call.params?.x === 10));
  assert.ok(mouse.some((call) => call.params?.type === "mousePressed"));
  assert.ok(mouse.some((call) => call.params?.type === "mouseReleased"));
  assert.ok(mouse.some((call) => call.params?.type === "mouseWheel"));
  const evaluations = calls.filter((call) => call.method === "Runtime.evaluate");
  assert.ok(
    evaluations.some((call) => String(call.params?.expression).includes('"#name"')),
  );
  assert.ok(
    evaluations.some((call) => String(call.params?.expression).includes("attached")),
  );
});
