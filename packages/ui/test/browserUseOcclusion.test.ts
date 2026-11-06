/**
 * Native-layer occlusion helpers.
 *
 * Run from `packages/ui`:
 *   node --import tsx --test test/browserUseOcclusion.test.ts
 */
import assert from "node:assert/strict";
import test from "node:test";
import {
  BROWSER_OCCLUDER_SELECTOR,
  occlusionFromRects,
  rectsOverlap,
  startBrowserOcclusionWatcher,
  styleOccludes,
  type BrowserOcclusionRect,
} from "../src/browser-use/browserUseOcclusion.js";

const surface: BrowserOcclusionRect = {
  left: 0,
  top: 0,
  right: 800,
  bottom: 600,
  width: 800,
  height: 600,
};

function rect(left: number, top: number, width: number, height: number): BrowserOcclusionRect {
  return { left, top, width, height, right: left + width, bottom: top + height };
}

test("rectsOverlap handles overlap, edges and empty rects", () => {
  assert.equal(rectsOverlap(surface, rect(10, 10, 100, 100)), true);
  assert.equal(rectsOverlap(surface, rect(700, 500, 200, 200)), true);
  assert.equal(rectsOverlap(surface, rect(800, 0, 100, 100)), false);
  assert.equal(rectsOverlap(surface, rect(0, 600, 100, 100)), false);
  assert.equal(rectsOverlap(surface, rect(900, 900, 100, 100)), false);
  assert.equal(rectsOverlap(surface, rect(10, 10, 0, 100)), false);
  assert.equal(rectsOverlap(surface, rect(10, 10, 100, 0)), false);
});

test("styleOccludes follows display, visibility and opacity", () => {
  assert.equal(styleOccludes({}), true);
  assert.equal(styleOccludes({ display: "none" }), false);
  assert.equal(styleOccludes({ visibility: "hidden" }), false);
  assert.equal(styleOccludes({ opacity: "0" }), false);
  assert.equal(styleOccludes({ opacity: "0.4" }), true);
  assert.equal(styleOccludes({ opacity: "not-a-number" }), true);
});

test("occlusionFromRects requires a visible surface and an intersecting overlay", () => {
  assert.equal(occlusionFromRects(null, [rect(0, 0, 10, 10)]), false);
  assert.equal(occlusionFromRects(surface, []), false);
  assert.equal(occlusionFromRects(surface, [rect(900, 900, 10, 10)]), false);
  assert.equal(
    occlusionFromRects(surface, [rect(900, 900, 10, 10), rect(10, 10, 10, 10)]),
    true,
  );
});

test("the occluder selector covers radix, dialogs and toasts", () => {
  for (const marker of [
    "[data-radix-popper-content-wrapper]",
    "[role='dialog']",
    "[role='menu']",
    "[role='tooltip']",
    "[data-sonner-toast]",
    "[data-browser-occluder]",
  ]) {
    assert.ok(BROWSER_OCCLUDER_SELECTOR.includes(marker), marker);
  }
});

test("the watcher flips only when an overlay intersects and restores on close", async () => {
  let overlayRect = rect(900, 900, 10, 10);
  const overlay = {
    hasAttribute: () => false,
    getBoundingClientRect: () => overlayRect,
  };
  const doc = {
    body: {},
    documentElement: {},
    querySelectorAll: () => [overlay],
    defaultView: {
      getComputedStyle: () => ({ display: "block", visibility: "visible", opacity: "1" }),
      addEventListener: () => {},
      removeEventListener: () => {},
    },
  };
  const container = {
    ownerDocument: doc,
    getBoundingClientRect: () => surface,
    contains: () => false,
  };
  const changes: boolean[] = [];
  const watcher = startBrowserOcclusionWatcher({
    container: container as unknown as HTMLElement,
    document: doc as unknown as Document,
    intervalMs: 10,
    onChange: (occluded) => changes.push(occluded),
  });
  assert.deepEqual(changes, []);
  overlayRect = rect(10, 10, 100, 100);
  watcher.refresh();
  await new Promise((resolve) => setTimeout(resolve, 40));
  assert.deepEqual(changes, [true]);
  overlayRect = rect(0, 0, 0, 0);
  watcher.refresh();
  await new Promise((resolve) => setTimeout(resolve, 40));
  assert.deepEqual(changes, [true, false]);
  watcher.dispose();
});
