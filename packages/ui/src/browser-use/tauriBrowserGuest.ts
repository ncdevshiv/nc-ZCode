/**
 * Tauri child-webview browser guest facade.
 *
 * `UnifiedBrowserView` is written against Electron's `<webview>` element. In the
 * Tauri shell the guest is a native child webview owned by Rust, so this module
 * augments the renderer container with the same imperative surface and dispatches
 * the same event names. The Electron path keeps using the real `<webview>`.
 *
 * Known limits (documented in the crate README): the child webview is a native
 * layer, so DOM popovers cannot overlay it, and agent CDP attachment still lives
 * in the TypeScript host.
 */
import { logger } from "@/logger.js";

const EVENT_NAME = "zcode:browser-view:event";
const DEFAULT_URL = "about:blank";

type TauriInvoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>;
type TauriListen = (
  name: string,
  handler: (event: { payload: unknown }) => void,
) => Promise<() => void>;

interface TauriGlobal {
  core?: { invoke?: TauriInvoke };
  event?: { listen?: TauriListen };
}

interface BrowserViewBounds {
  x: number;
  y: number;
  width: number;
  height: number;
}

interface BrowserViewEventPayload {
  key: string;
  type: string;
  url?: string;
  title?: string;
  favicons?: string[];
}

interface BrowserViewStatePayload {
  url?: string;
  title?: string;
  canGoBack?: boolean;
  canGoForward?: boolean;
}

export interface TauriBrowserGuestController {
  setVisible(visible: boolean): void;
  /**
   * Hide the native webview while a DOM overlay covers it. The shell keeps the
   * requested visibility separately, so `setOccluded(false)` restores it.
   */
  setOccluded(occluded: boolean): void;
  dispose(): void;
}

function tauriGlobal(): TauriGlobal | undefined {
  return (globalThis as { __TAURI__?: TauriGlobal }).__TAURI__;
}

export function isTauriBrowserRuntime(): boolean {
  const tauri = tauriGlobal();
  return typeof tauri?.core?.invoke === "function" && typeof tauri?.event?.listen === "function";
}

function invoke(): TauriInvoke {
  const fn = tauriGlobal()?.core?.invoke;
  if (!fn) throw new Error("Tauri invoke is unavailable");
  return fn;
}

function listen(): TauriListen {
  const fn = tauriGlobal()?.event?.listen;
  if (!fn) throw new Error("Tauri event listener is unavailable");
  return fn;
}

function asEventPayload(value: unknown): BrowserViewEventPayload | null {
  if (!value || typeof value !== "object") return null;
  const record = value as Record<string, unknown>;
  if (typeof record.key !== "string" || typeof record.type !== "string") return null;
  const payload: BrowserViewEventPayload = { key: record.key, type: record.type };
  if (typeof record.url === "string") payload.url = record.url;
  if (typeof record.title === "string") payload.title = record.title;
  if (Array.isArray(record.favicons)) {
    payload.favicons = record.favicons.filter(
      (item): item is string => typeof item === "string",
    );
  }
  return payload;
}

function asStatePayload(value: unknown): BrowserViewStatePayload {
  if (!value || typeof value !== "object") return {};
  const record = value as Record<string, unknown>;
  return {
    ...(typeof record.url === "string" ? { url: record.url } : {}),
    ...(typeof record.title === "string" ? { title: record.title } : {}),
    ...(typeof record.canGoBack === "boolean" ? { canGoBack: record.canGoBack } : {}),
    ...(typeof record.canGoForward === "boolean" ? { canGoForward: record.canGoForward } : {}),
  };
}

function readBounds(container: HTMLElement): BrowserViewBounds {
  const rect = container.getBoundingClientRect();
  return {
    x: rect.left,
    y: rect.top,
    width: Math.max(1, rect.width),
    height: Math.max(1, rect.height),
  };
}

function dispatchGuestEvent(
  container: HTMLElement,
  type: string,
  properties: Record<string, unknown> = {},
): void {
  const event = new Event(type);
  Object.assign(event, properties);
  container.dispatchEvent(event);
}

function parseEvalResult(raw: unknown): unknown {
  if (typeof raw !== "string") return raw;
  try {
    return JSON.parse(raw) as unknown;
  } catch {
    return raw;
  }
}

/**
 * Attach the Tauri guest facade to `container`. The container is augmented in
 * place so the existing `ElectronWebviewTag` consumers (including the screenshot
 * hook's `getBoundingClientRect`/`dataset` reads) keep working.
 */
export function attachTauriBrowserGuest(
  container: HTMLElement,
  options: { key: string; visible: boolean; initialUrl?: string },
): TauriBrowserGuestController {
  const { key } = options;
  const invokeCommand = invoke();
  let disposed = false;
  let attached = false;
  let visible = options.visible;
  let occluded = false;
  let currentUrl = options.initialUrl ?? DEFAULT_URL;
  let title = "";
  let webContentsId = 0;
  let canGoBack = false;
  let canGoForward = false;
  let frame = 0;
  let unlisten: (() => void) | undefined;

  const guest = container as unknown as ElectronWebviewTag;
  guest.getURL = () => currentUrl;
  guest.getTitle = () => title;
  guest.canGoBack = () => canGoBack;
  guest.canGoForward = () => canGoForward;
  guest.getWebContentsId = () => webContentsId;
  guest.loadURL = async (url: string) => {
    await invokeCommand("browser_view_navigate", { key, url });
    currentUrl = url;
  };
  guest.goBack = () => {
    void invokeCommand("browser_view_go_back", { key }).then(refreshState);
  };
  guest.goForward = () => {
    void invokeCommand("browser_view_go_forward", { key }).then(refreshState);
  };
  guest.reload = () => {
    void invokeCommand("browser_view_reload", { key });
  };
  guest.setZoomFactor = (factor: number) => {
    void invokeCommand("browser_view_set_zoom", { key, factor });
  };
  guest.executeJavaScript = async (script: string) =>
    parseEvalResult(await invokeCommand("browser_view_eval", { key, script }));
  guest.openDevTools = () => {
    void invokeCommand("browser_view_open_devtools", { key });
  };

  async function refreshState(): Promise<void> {
    if (disposed) return;
    try {
      const state = asStatePayload(await invokeCommand("browser_view_state", { key }));
      if (typeof state.url === "string" && state.url.length > 0) currentUrl = state.url;
      if (typeof state.title === "string") title = state.title;
      if (typeof state.canGoBack === "boolean") canGoBack = state.canGoBack;
      if (typeof state.canGoForward === "boolean") canGoForward = state.canGoForward;
    } catch (error) {
      logger.debug("[browser-use] tauri guest state unavailable", {
        error: error instanceof Error ? error.message : String(error),
        key,
      });
    }
  }

  async function applyBounds(): Promise<void> {
    if (disposed) return;
    const bounds = readBounds(container);
    try {
      await invokeCommand("browser_view_set_bounds", { key, bounds });
      await invokeCommand("browser_view_set_visible", {
        key,
        visible: visible && bounds.width > 1 && bounds.height > 1,
      });
    } catch (error) {
      logger.debug("[browser-use] tauri guest bounds update failed", {
        error: error instanceof Error ? error.message : String(error),
        key,
      });
    }
  }

  function scheduleBounds(): void {
    if (disposed || frame !== 0) return;
    frame = requestAnimationFrame(() => {
      frame = 0;
      void applyBounds();
    });
  }

  const resizeObserver =
    typeof ResizeObserver === "undefined" ? null : new ResizeObserver(scheduleBounds);
  resizeObserver?.observe(container);
  window.addEventListener("resize", scheduleBounds);
  document.addEventListener("scroll", scheduleBounds, true);

  void listen()(EVENT_NAME, (event) => {
    if (disposed) return;
    const payload = asEventPayload(event.payload);
    if (!payload || payload.key !== key) return;
    switch (payload.type) {
      case "did-start-loading":
        dispatchGuestEvent(container, "did-start-loading");
        break;
      case "dom-ready":
        if (!attached) {
          attached = true;
          dispatchGuestEvent(container, "did-attach");
        }
        if (payload.url) currentUrl = payload.url;
        dispatchGuestEvent(container, "dom-ready");
        void refreshState();
        break;
      case "did-navigate":
        if (payload.url) currentUrl = payload.url;
        dispatchGuestEvent(container, "did-navigate", { url: currentUrl, isMainFrame: true });
        void refreshState();
        break;
      case "did-stop-loading":
        dispatchGuestEvent(container, "did-stop-loading");
        void refreshState();
        break;
      case "page-title-updated":
        if (payload.title !== undefined) title = payload.title;
        dispatchGuestEvent(container, "page-title-updated", { title, explicitSet: false });
        break;
      case "page-favicon-updated":
        dispatchGuestEvent(container, "page-favicon-updated", {
          favicons: payload.favicons ?? [],
        });
        break;
      default:
        break;
    }
  }).then((dispose) => {
    if (disposed) dispose();
    else unlisten = dispose;
  });

  void invokeCommand("browser_view_create", {
    payload: {
      key,
      url: currentUrl,
      bounds: readBounds(container),
      visible,
    },
  })
    .then((created) => {
      if (disposed) {
        void invokeCommand("browser_view_close", { key }).catch(() => {
          // The guest was closed while creation was still in flight.
        });
        return;
      }
      const result = created as { webviewId?: unknown };
      if (typeof result.webviewId === "number") webContentsId = result.webviewId;
    })
    .catch((error: unknown) => {
      const message = error instanceof Error ? error.message : String(error);
      logger.warn("[browser-use] tauri guest create failed", { key, message });
      dispatchGuestEvent(container, "did-fail-load", {
        errorCode: -1,
        errorDescription: message,
        validatedURL: currentUrl,
        isMainFrame: true,
      });
    });

  return {
    setVisible(nextVisible: boolean) {
      visible = nextVisible;
      void applyBounds();
    },
    setOccluded(nextOccluded: boolean) {
      if (occluded === nextOccluded) return;
      occluded = nextOccluded;
      void invokeCommand("browser_view_set_occluded", {
        key,
        occluded: nextOccluded,
      }).catch((error: unknown) => {
        logger.debug("[browser-use] tauri guest occlusion update failed", {
          error: error instanceof Error ? error.message : String(error),
          key,
        });
      });
    },
    dispose() {
      disposed = true;
      if (frame !== 0) cancelAnimationFrame(frame);
      resizeObserver?.disconnect();
      window.removeEventListener("resize", scheduleBounds);
      document.removeEventListener("scroll", scheduleBounds, true);
      unlisten?.();
      void invokeCommand("browser_view_close", { key }).catch(() => {
        // The guest may already be gone (window closing); nothing to recover.
      });
    },
  };
}
