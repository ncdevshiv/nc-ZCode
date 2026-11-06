/**
 * Native-layer occlusion for the Tauri browser pane.
 *
 * The browser pane is a native child webview above the DOM, so Radix menus,
 * dialogs, selects, tooltips and toasts cannot paint over it. This module
 * watches for those overlays and reports when one intersects the pane; the
 * Tauri guest controller then hides the native webview until the overlay closes.
 *
 * The shell keeps the tab's requested visibility and the occlusion flag
 * separately (`browser_view_set_occluded`), so clearing the overlay restores
 * exactly the previous state.
 */

export interface BrowserOcclusionRect {
  left: number;
  top: number;
  right: number;
  bottom: number;
  width: number;
  height: number;
}

export interface BrowserOcclusionStyle {
  display?: string;
  visibility?: string;
  opacity?: string;
}

/** Elements that render above the native webview when open. */
export const BROWSER_OCCLUDER_SELECTOR = [
  "[data-radix-popper-content-wrapper]",
  "[data-radix-menu-content]",
  "[data-radix-select-content]",
  "[role='dialog']",
  "[role='alertdialog']",
  "[role='menu']",
  "[role='listbox']",
  "[role='tooltip']",
  "[data-sonner-toast]",
  "[data-browser-occluder]",
].join(",");

export function rectsOverlap(
  left: BrowserOcclusionRect,
  right: BrowserOcclusionRect,
): boolean {
  return (
    left.width > 0 &&
    left.height > 0 &&
    right.width > 0 &&
    right.height > 0 &&
    left.left < right.right &&
    left.right > right.left &&
    left.top < right.bottom &&
    left.bottom > right.top
  );
}

export function styleOccludes(style: BrowserOcclusionStyle): boolean {
  if (style.display === "none" || style.visibility === "hidden") return false;
  const opacity = Number(style.opacity ?? "1");
  return Number.isFinite(opacity) ? opacity > 0 : true;
}

export function occlusionFromRects(
  surface: BrowserOcclusionRect | null,
  occluders: readonly BrowserOcclusionRect[],
): boolean {
  if (!surface) return false;
  return occluders.some((rect) => rectsOverlap(surface, rect));
}

export interface BrowserOcclusionWatcher {
  refresh(): void;
  dispose(): void;
}

/**
 * Observe the document for visible overlays intersecting `container`.
 *
 * Scans on a 120 ms interval (overlay lifecycle is short and the query set is
 * tiny), on DOM mutations, and on scroll/resize. The callback only fires when
 * the boolean changes.
 */
export function startBrowserOcclusionWatcher(input: {
  container: HTMLElement;
  onChange: (occluded: boolean) => void;
  document?: Document;
  intervalMs?: number;
}): BrowserOcclusionWatcher {
  const doc = input.document ?? input.container.ownerDocument;
  const view = doc.defaultView;
  const intervalMs = input.intervalMs ?? 120;
  let disposed = false;
  let current = false;
  let timer: ReturnType<typeof setInterval> | null = null;
  let frame = 0;
  const observers: MutationObserver[] = [];
  const timeouts: ReturnType<typeof setTimeout>[] = [];

  const emit = (next: boolean) => {
    if (disposed || next === current) return;
    current = next;
    input.onChange(next);
  };

  const scan = () => {
    if (disposed) return;
    const surfaceRect = input.container.getBoundingClientRect();
    const surface: BrowserOcclusionRect | null =
      surfaceRect.width > 0 && surfaceRect.height > 0
        ? {
            left: surfaceRect.left,
            top: surfaceRect.top,
            right: surfaceRect.right,
            bottom: surfaceRect.bottom,
            width: surfaceRect.width,
            height: surfaceRect.height,
          }
        : null;
    if (!surface) {
      emit(false);
      return;
    }
    const elements = doc.querySelectorAll(BROWSER_OCCLUDER_SELECTOR);
    const occluders: BrowserOcclusionRect[] = [];
    for (const element of elements) {
      if (input.container.contains(element)) continue;
      if (element.hasAttribute("data-browser-occlusion-ignore")) continue;
      const style = view?.getComputedStyle(element);
      if (style && !styleOccludes(style)) continue;
      const rect = element.getBoundingClientRect();
      if (rect.width <= 0 || rect.height <= 0) continue;
      occluders.push(rect);
      if (occluders.length >= 200) break;
    }
    emit(occlusionFromRects(surface, occluders));
  };

  const schedule = () => {
    if (disposed || frame !== 0) return;
    const raf = view?.requestAnimationFrame?.bind(view);
    if (raf) {
      frame = raf(() => {
        frame = 0;
        scan();
      });
    } else {
      scan();
    }
    // Poppers are positioned after mount; re-check a few times.
    for (const delay of [50, 160, 320]) {
      timeouts.push(
        setTimeout(() => {
          if (!disposed) scan();
        }, delay),
      );
    }
  };

  if (typeof MutationObserver !== "undefined") {
    const observer = new MutationObserver(schedule);
    observer.observe(doc.body ?? doc.documentElement, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ["class", "style", "data-state", "hidden", "aria-hidden"],
    });
    observers.push(observer);
  }
  view?.addEventListener("scroll", schedule, true);
  view?.addEventListener("resize", schedule);
  timer = setInterval(scan, intervalMs);
  scan();

  return {
    refresh: schedule,
    dispose() {
      disposed = true;
      if (timer) clearInterval(timer);
      if (frame !== 0) view?.cancelAnimationFrame?.(frame);
      for (const timeout of timeouts) clearTimeout(timeout);
      for (const observer of observers) observer.disconnect();
      view?.removeEventListener("scroll", schedule, true);
      view?.removeEventListener("resize", schedule);
    },
  };
}
