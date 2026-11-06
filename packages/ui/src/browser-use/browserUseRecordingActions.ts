/**
 * Scripted recording actions + cursor overlay.
 *
 * The capture port records a native child webview over CDP. Actions are
 * dispatched through the same CDP channel, and the cursor overlay is injected
 * into the guest page so the recorded frames show the pointer (the TS manager
 * does the same through `executeJavaScript`).
 *
 * Selectors are embedded with `JSON.stringify`, so quotes/backslashes in a
 * selector or typed text cannot break out of the generated expression.
 */
import type { BrowserRecordingAction } from "@zcode/shared";

export type RecordingCdpCall = (
  method: string,
  params?: Record<string, unknown>,
) => Promise<unknown>;

export interface RecordingActionContext {
  cdp: RecordingCdpCall;
  isCancelled: () => boolean;
  sleep: (milliseconds: number) => Promise<void>;
  viewport: { width: number; height: number };
}

export class RecordingCancelledError extends Error {
  constructor() {
    super("browser recording cancelled");
    this.name = "RecordingCancelledError";
  }
}

export const CURSOR_OVERLAY_ID = "__zcode_browser_recording_cursor";

export function cursorOverlayScript(): string {
  return String.raw`(() => {
  const id = "${CURSOR_OVERLAY_ID}";
  document.getElementById(id)?.remove();
  const cursor = document.createElement("div");
  cursor.id = id;
  cursor.style.cssText = "position:fixed;left:0;top:0;width:18px;height:18px;border-radius:50%;background:#ff4d4f;border:2px solid #fff;box-shadow:0 1px 5px rgba(0,0,0,.45);pointer-events:none;z-index:2147483647;transform:translate(-50%,-50%);opacity:0;transition:opacity 80ms linear";
  document.documentElement.appendChild(cursor);
  const move = (event) => {
    cursor.style.left = event.clientX + "px";
    cursor.style.top = event.clientY + "px";
    cursor.style.opacity = "1";
  };
  const down = () => {
    cursor.style.transform = "translate(-50%,-50%) scale(.72)";
  };
  const up = () => {
    cursor.style.transform = "translate(-50%,-50%) scale(1)";
  };
  window.__zcodeBrowserRecordingCursorCleanup?.();
  window.addEventListener("mousemove", move, true);
  window.addEventListener("mousedown", down, true);
  window.addEventListener("mouseup", up, true);
  window.__zcodeBrowserRecordingCursorCleanup = () => {
    window.removeEventListener("mousemove", move, true);
    window.removeEventListener("mousedown", down, true);
    window.removeEventListener("mouseup", up, true);
    cursor.remove();
    delete window.__zcodeBrowserRecordingCursorCleanup;
  };
  return true;
})()`;
}

export function cursorCleanupScript(): string {
  return `window.__zcodeBrowserRecordingCursorCleanup?.()`;
}

export function selectorPointScript(selector: string): string {
  return `(() => {
  const el = document.querySelector(${JSON.stringify(selector)});
  if (!el) return null;
  const rect = el.getBoundingClientRect();
  return { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };
})()`;
}

export function selectorScrollTargetScript(selector: string): string {
  return `(() => {
  const el = document.querySelector(${JSON.stringify(selector)});
  if (!el) return null;
  const rect = el.getBoundingClientRect();
  return { x: window.scrollX + rect.left, y: window.scrollY + rect.top };
})()`;
}

export function fillSelectorScript(selector: string, text: string): string {
  return `(() => {
  const el = document.querySelector(${JSON.stringify(selector)});
  if (!el) return false;
  el.focus();
  const text = ${JSON.stringify(text)};
  if (el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement) {
    const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    const setter = Object.getOwnPropertyDescriptor(proto, "value")?.set;
    if (setter) setter.call(el, text); else el.value = text;
    el.dispatchEvent(new Event("input", { bubbles: true }));
    el.dispatchEvent(new Event("change", { bubbles: true }));
  } else if (el.isContentEditable) {
    el.textContent = text;
    el.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: text }));
  } else {
    el.value = text;
    el.dispatchEvent(new Event("input", { bubbles: true }));
  }
  return true;
})()`;
}

export function waitForSelectorScript(selector: string, state: string): string {
  return `(() => {
  const el = document.querySelector(${JSON.stringify(selector)});
  if (!el) return { attached: false, visible: false, state: ${JSON.stringify(state)} };
  const style = getComputedStyle(el);
  const rect = el.getBoundingClientRect();
  const visible = style.display !== "none" && style.visibility !== "hidden" &&
    Number(style.opacity) !== 0 && rect.width > 0 && rect.height > 0;
  return { attached: true, visible, state: ${JSON.stringify(state)} };
})()`;
}

async function evaluate(
  ctx: RecordingActionContext,
  expression: string,
): Promise<unknown> {
  const result = (await ctx.cdp("Runtime.evaluate", {
    expression,
    returnByValue: true,
    awaitPromise: true,
    userGesture: true,
  })) as {
    result?: { value?: unknown };
    exceptionDetails?: { text?: string; exception?: { description?: string } };
  };
  if (result?.exceptionDetails) {
    throw new Error(
      result.exceptionDetails.exception?.description ??
        result.exceptionDetails.text ??
        "recording action evaluation failed",
    );
  }
  return result?.result?.value;
}

function assertActive(ctx: RecordingActionContext): void {
  if (ctx.isCancelled()) throw new RecordingCancelledError();
}

function readPoint(value: unknown): { x: number; y: number } | null {
  if (!value || typeof value !== "object") return null;
  const point = value as { x?: unknown; y?: unknown };
  return typeof point.x === "number" && typeof point.y === "number"
    ? { x: point.x, y: point.y }
    : null;
}

function readScroll(value: unknown): { x: number; y: number } {
  const point = readPoint(value);
  return point ?? { x: 0, y: 0 };
}

async function resolveSelectorPoint(
  ctx: RecordingActionContext,
  selector: string,
): Promise<{ x: number; y: number }> {
  const point = readPoint(await evaluate(ctx, selectorPointScript(selector)));
  if (!point) throw new Error(`recording selector '${selector}' has no visible point`);
  return point;
}

async function movePointer(
  ctx: RecordingActionContext,
  pointer: { x: number; y: number },
  x: number,
  y: number,
  durationMs: number,
): Promise<void> {
  const steps = Math.max(1, Math.min(60, Math.round(durationMs / 16)));
  const startX = pointer.x;
  const startY = pointer.y;
  for (let index = 1; index <= steps; index += 1) {
    assertActive(ctx);
    const progress = index / steps;
    await ctx.cdp("Input.dispatchMouseEvent", {
      type: "mouseMoved",
      x: startX + (x - startX) * progress,
      y: startY + (y - startY) * progress,
    });
    if (durationMs > 0 && index < steps) {
      await ctx.sleep(durationMs / steps);
    }
  }
  pointer.x = x;
  pointer.y = y;
}

async function scrollBy(
  ctx: RecordingActionContext,
  pointer: { x: number; y: number },
  deltaX: number,
  deltaY: number,
  durationMs: number,
): Promise<void> {
  const steps = Math.max(1, Math.min(60, Math.round(durationMs / 16)));
  for (let index = 0; index < steps; index += 1) {
    assertActive(ctx);
    await ctx.cdp("Input.dispatchMouseEvent", {
      type: "mouseWheel",
      x: pointer.x,
      y: pointer.y,
      deltaX: deltaX / steps,
      deltaY: deltaY / steps,
    });
    if (durationMs > 0 && index + 1 < steps) {
      await ctx.sleep(durationMs / steps);
    }
  }
}

async function executeAction(
  ctx: RecordingActionContext,
  action: BrowserRecordingAction,
  pointer: { x: number; y: number },
): Promise<void> {
  assertActive(ctx);
  if (action.type === "wait") {
    await ctx.sleep(action.durationMs);
    return;
  }
  if (action.type === "click") {
    const point =
      action.selector !== undefined
        ? await resolveSelectorPoint(ctx, action.selector)
        : typeof action.x === "number" && typeof action.y === "number"
          ? { x: action.x, y: action.y }
          : null;
    if (!point) throw new Error("recording click requires selector or (x,y)");
    await movePointer(ctx, pointer, point.x, point.y, 0);
    const button = action.button ?? "left";
    const clicks = action.doubleClick ? 2 : 1;
    for (let click = 1; click <= clicks; click += 1) {
      assertActive(ctx);
      await ctx.cdp("Input.dispatchMouseEvent", {
        type: "mousePressed",
        x: point.x,
        y: point.y,
        button,
        clickCount: click,
      });
      await ctx.cdp("Input.dispatchMouseEvent", {
        type: "mouseReleased",
        x: point.x,
        y: point.y,
        button,
        clickCount: click,
      });
    }
    return;
  }
  if (action.type === "type") {
    const filled = await evaluate(
      ctx,
      fillSelectorScript(action.selector, action.text),
    );
    if (filled !== true) {
      throw new Error(`recording selector '${action.selector}' is not fillable`);
    }
    return;
  }
  if (action.type === "hover") {
    const point =
      action.selector !== undefined
        ? await resolveSelectorPoint(ctx, action.selector)
        : typeof action.x === "number" && typeof action.y === "number"
          ? { x: action.x, y: action.y }
          : null;
    if (!point) throw new Error("recording hover requires selector or (x,y)");
    await movePointer(ctx, pointer, point.x, point.y, action.durationMs ?? 0);
    return;
  }
  if (action.type === "move") {
    await movePointer(ctx, pointer, action.x, action.y, action.durationMs ?? 0);
    return;
  }
  if (action.type === "scroll") {
    await scrollBy(
      ctx,
      pointer,
      action.deltaX ?? 0,
      action.deltaY,
      action.durationMs ?? 0,
    );
    return;
  }
  if (action.type === "scrollTo") {
    const current = readScroll(
      await evaluate(ctx, "({ x: window.scrollX, y: window.scrollY })"),
    );
    const target = action.selector
      ? readPoint(
          await evaluate(ctx, selectorScrollTargetScript(action.selector)),
        )
      : {
          x: action.x ?? current.x,
          y: action.y ?? current.y,
        };
    if (!target) {
      throw new Error(`recording selector '${action.selector}' has no scroll target`);
    }
    await scrollBy(
      ctx,
      pointer,
      target.x - current.x,
      target.y - current.y,
      action.durationMs ?? 0,
    );
    return;
  }
  if (action.type === "wheel") {
    const times = action.times ?? 1;
    for (let index = 0; index < times; index += 1) {
      assertActive(ctx);
      await ctx.cdp("Input.dispatchMouseEvent", {
        type: "mouseWheel",
        x: pointer.x,
        y: pointer.y,
        deltaX: action.deltaX ?? 0,
        deltaY: action.deltaY,
      });
      if (action.intervalMs) await ctx.sleep(action.intervalMs);
    }
    return;
  }
  if (action.type === "drag") {
    const first = action.path[0]!;
    const last = action.path.at(-1)!;
    const steps = Math.max(1, action.path.length - 1);
    const intervalMs = Math.floor((action.durationMs ?? 0) / steps);
    await movePointer(ctx, pointer, first.x, first.y, 0);
    await ctx.cdp("Input.dispatchMouseEvent", {
      type: "mousePressed",
      x: first.x,
      y: first.y,
      button: "left",
      clickCount: 1,
    });
    for (const point of action.path.slice(1)) {
      assertActive(ctx);
      await ctx.cdp("Input.dispatchMouseEvent", {
        type: "mouseMoved",
        x: point.x,
        y: point.y,
        button: "left",
        buttons: 1,
      });
      if (intervalMs > 0) await ctx.sleep(intervalMs);
    }
    await ctx.cdp("Input.dispatchMouseEvent", {
      type: "mouseReleased",
      x: last.x,
      y: last.y,
      button: "left",
      clickCount: 1,
    });
    pointer.x = last.x;
    pointer.y = last.y;
    return;
  }
  if (action.type === "waitFor") {
    const timeoutMs = action.timeoutMs ?? 3_000;
    const deadline = Date.now() + timeoutMs;
    const state = action.state ?? "visible";
    while (Date.now() < deadline) {
      assertActive(ctx);
      const probe = (await evaluate(
        ctx,
        waitForSelectorScript(action.selector, state),
      )) as { attached?: unknown; visible?: unknown } | null;
      const attached = probe?.attached === true;
      const visible = probe?.visible === true;
      const satisfied =
        state === "attached"
          ? attached
          : state === "detached"
            ? !attached
            : state === "hidden"
              ? attached && !visible
              : attached && visible;
      if (satisfied) return;
      await ctx.sleep(100);
    }
    throw new Error(`recording waitFor '${action.selector}' timed out`);
  }
}

export async function executeRecordingActions(
  actions: readonly BrowserRecordingAction[],
  ctx: RecordingActionContext,
): Promise<void> {
  const pointer = {
    x: ctx.viewport.width / 2,
    y: ctx.viewport.height / 2,
  };
  for (const action of actions) {
    await executeAction(ctx, action, pointer);
    const delayAfterMs = "delayAfterMs" in action ? action.delayAfterMs : undefined;
    if (delayAfterMs) await ctx.sleep(delayAfterMs);
  }
}
