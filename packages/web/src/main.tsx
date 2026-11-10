/* eslint-disable max-lines -- Web å…¥å£é›†ä¸­ç¼–æŽ’å¯åŠ¨ã€è·¯ç”±ä¸Ž workspace shell wiringï¼Œä¸Ž Root.tsx åŒæ ·å…ˆä¿æŒå…¥å£æ”¶å£ï¼Œé¿å…è·¨å±‚çŠ¶æ€æ‹†æ•£ã€‚ */
import { createRoot } from "react-dom/client";
import {
  AppErrorBoundary,
  NcagApp,
  Root,
  ZCodeIntlProvider,
  generateMobileDeviceFingerprint,
  getBrowserUseLifecycleController,
  playTaskNotificationSound,
  setStreamClientId,
  type Theme,
} from "@zcode/ui";
import "@zcode/ui/styles.css";
import { connectViaWebSocket } from "@zcode/client";
import { WebCallbackPage } from "./auth/WebCallbackPage.js";
import { createWebAuthService } from "./auth/webAuthService.js";
import { WEB_ZAI_OAUTH_CONFIG, resolveWebAuthDevReturnTo } from "./auth/webZaiOAuthConfig.js";
import { parseOAuthState, resolveSafeAppReturnTo } from "./auth/oauthStateCodec.js";
import { resolveWebCommunityUrl, resolveWebHelpConfig } from "./communityUrl.js";
import {
  ConversationShareLandingLoader,
  ConversationShareLandingStatus,
} from "./share/ConversationShareLandingPage.js";
import {
  ConversationSharePreviewClient,
  resolveConversationShareRouteLocale,
} from "./share/conversationSharePreviewClient.js";
import {
  isConversationSharePath,
  resolveConversationShareCodeFromPath,
} from "./share/conversationShareRoute.js";
import type {
  BrowserGuestAttachRejectReason,
  BrowserRecordingOptions,
  ChromeBrowserDataImportResult,
  DockerContainerInfo,
  IPlatformService,
  Locale,
  RemoteTarget,
  SaveFileResult,
  ServerRemoteInfo,
  SSHConfigAliasOption,
  WSLDistro,
} from "@zcode/shared";
import { WEB_DEFAULT_THEME, resolveWebInitialTheme } from "./webThemeSeed.js";
import { createRemoteWorkspaceClient } from "./tauriRemote.js";

function resolveWebThemePreference(defaultTheme: Theme = WEB_DEFAULT_THEME): Theme {
  const saved = localStorage.getItem("zcode-theme");
  return resolveWebInitialTheme({ storedTheme: saved, defaultTheme });
}

// åˆå§‹åŒ–ä¸»é¢˜ï¼šé»˜è®¤ Zai darkï¼ŒåŽç»­ç”± useTheme hook æŽ¥ç®¡
// system æ¨¡å¼ä¸‹éœ€è¦æŸ¥è¯¢ç³»ç»Ÿåå¥½ï¼›éž system æ¨¡å¼ç›´æŽ¥ç”¨å­˜å‚¨å€¼
{
  // åˆ†äº«é¡µæ²¡æœ‰æœ¬åœ°ä¸»é¢˜é…ç½®æ—¶ä½¿ç”¨æµ…è‰²ï¼Œå·²æœ‰é…ç½®ä»ç„¶æ²¿ç”¨ï¼›å…¶ä»– Web é¡µé¢ç»§ç»­é»˜è®¤æ·±è‰²ã€‚
  const saved = resolveWebThemePreference(
    isConversationSharePath(window.location.pathname) ? "zai-light" : undefined,
  );
  const resolved =
    saved === "system"
      ? window.matchMedia("(prefers-color-scheme: dark)").matches
        ? "dark"
        : "light"
      : saved === "dark" || saved === "zai-dark"
        ? "dark"
        : "light";
  const appliedTheme =
    saved === "system"
      ? resolved === "dark"
        ? "zai-dark"
        : "zai-light"
      : saved === "dark"
        ? "zai-dark"
        : saved === "light"
          ? "zai-light"
          : saved;
  document.documentElement.classList.toggle("dark", resolved === "dark");
  document.documentElement.classList.toggle("theme-zai-light", appliedTheme === "zai-light");
  document.documentElement.classList.toggle("theme-zai-dark", appliedTheme === "zai-dark");
}

async function resolveFeedbackUrl(): Promise<string | undefined> {
  return (await resolveWebHelpConfig()).feedback_url;
}

const root = createRoot(document.getElementById("root")!);
const webAuthService = createWebAuthService();

// åˆå§‹åŒ– Web ç«¯æµå¼ clientIdï¼Œç¡®ä¿æ‰€æœ‰ hook åœ¨é¦–æ¬¡æ¸²æŸ“å‰å°±ä½¿ç”¨ç¨³å®š ID
{
  setStreamClientId(generateMobileDeviceFingerprint());
}

interface WebBootstrapResult {
  wsUrl: string;
  initialWorkspaceAbsPath?: string;
  initialWorkspaceIdentity?: string;
  initialTaskId?: string;
  restoreSession?: boolean;
  allowOpenWorkspace?: boolean;
}

function isWebOAuthCallback(params: URLSearchParams): boolean {
  // OAuth disabled
  return false;
}

function renderWebAuthCallbackPage(): void {
  // OAuth disabled
  window.location.replace("/");
}

async function renderConversationSharePage(): Promise<void> {
  // é¡µé¢è¯­è¨€è·Ÿéšè·¯å¾„å‰ç¼€ï¼š/cn/share ä¸­æ–‡ï¼Œè£¸ /share è‹±æ–‡ã€‚
  const routeLocale = resolveConversationShareRouteLocale(window.location.pathname);
  // index.html å›ºå®š lang="en"ï¼›ä¸åŒæ­¥ä¼šè®©ä¸­æ–‡åˆ†äº«é¡µå¯¹æ— éšœç¢ä¸Žæµè§ˆå™¨ç¿»è¯‘éƒ½æŠ¥é”™è¯­è¨€ã€‚
  document.documentElement.lang = routeLocale;
  // åˆ†äº«é¡µå¿…é¡»è®¾ç½® titleï¼šå¦åˆ™æµè§ˆå™¨æ ‡ç­¾åªæ˜¾ç¤º index.html çš„é€šç”¨æ ‡é¢˜ã€‚
  // ä¼šè¯æ ‡é¢˜è¦ç­‰ preview åŠ è½½å®Œï¼Œå…ˆç»™ä¸€ä¸ªè¯­è¨€æ­£ç¡®çš„å…œåº•ã€‚
  document.title = routeLocale === "zh-CN" ? "ZCode ä¼šè¯åˆ†äº«" : "ZCode Conversation Share";
  const shareCode = resolveConversationShareCodeFromPath(window.location.pathname);
  if (!shareCode) {
    root.render(
      <ConversationShareLandingStatus
        state={{ kind: "error", error: "invalid_contract" }}
        locale={routeLocale}
      />,
    );
    return;
  }

  const endpointOrigin =
    import.meta.env.VITE_ZCODE_BASE_URL?.trim().replace(/\/+$/u, "") || window.location.origin;
  const mockMode =
    import.meta.env.DEV && import.meta.env.VITE_CONVERSATION_SHARE_PREVIEW_MOCK === "true";
  // Share åŠ è½½å¤±è´¥ä¸èƒ½åªæœ‰é€šç”¨ network æ–‡æ¡ˆï¼šéœ€è¦åŒºåˆ† mockã€endpoint é…ç½®æˆ–è·¨åŸŸ fetchã€‚
  // è¿™é‡Œåªè®°å½•è¿è¡Œæ—¶è·¯ç”±ä¸Ž endpointï¼Œä¸è®°å½•å®Œæ•´ pathnameï¼Œé¿å…æŠŠ share code å†™å…¥æ—¥å¿—ã€‚
  console.info("[conversation-share-web]", "preview_runtime_initialized", {
    browserOrigin: window.location.origin,
    routeKind: "canonical",
    endpointOrigin,
    transport: mockMode ? "mock" : "fetch",
  });
  const client = mockMode
    ? new (
        await import("./share/mockConversationSharePreviewClient.js")
      ).MockConversationSharePreviewClient()
    : new ConversationSharePreviewClient({ baseUrl: `${endpointOrigin}/api/v1` });
  const getMockToken = () =>
    mockMode && window.sessionStorage.getItem("zcode:share:mock-auth") === "owner"
      ? "mock-owner-token"
      : null;
  const onLogout = () => {
    if (mockMode) {
      window.sessionStorage.removeItem("zcode:share:mock-auth");
      window.location.reload();
      return;
    }
    void webAuthService.logout();
  };
  root.render(
    <ConversationShareLandingLoader
      shareCode={shareCode}
      client={client}
      getAccessToken={() => getMockToken() ?? webAuthService.getZCodeJwtToken()}
      onLogin={() => {
        // OAuth disabled
        if (mockMode) {
          window.sessionStorage.setItem("zcode:share:mock-auth", "owner");
          window.location.reload();
          return;
        }
        window.location.replace("/");
      }}
      onLogout={onLogout}
      locale={routeLocale}
      theme={resolveWebThemePreference("zai-light")}
    />,
  );
}

function createWebPlatform(): IPlatformService {
  return {
    canSelectFilePath: false,
    // Web ç«¯æ— æ³•æ‰“å¼€ç³»ç»Ÿç›®å½•é€‰æ‹©æ¡†
    selectDirectory: () => Promise.resolve(null),
    // Web ç«¯æ— æ³•æ‰“å¼€ç³»ç»Ÿæ–‡ä»¶é€‰æ‹©æ¡†
    selectFile: () => Promise.resolve(null),
    selectFiles: () => Promise.resolve([]),
    getPathForFile: () => null,
    createTempTextAttachment: () =>
      Promise.reject(new Error("Temporary text attachments require a desktop host")),
    onRemoteConnectionLog: () => () => {},
    onRemoteSessionClosed: () => () => {},
    // Web ç«¯æ— å¤šçª—å£ç®¡ç†
    activateOrSetWorkspace: () => Promise.resolve({ activated: false }),
    // 普通 Web 模式（浏览器直连）只保证 server 本地工作区可用：远程 WebSocket 只暴露
    // 部分 service，与 Root/RemoteServiceAccess 需要的完整 accessor 不匹配，直接打开
    // ?remote=<id> 会在项目向导或首屏卡住。桌面端（Tauri）的 connectRemote 走内嵌
    // Rust 服务的 /api/remote/* 真实路由。
    connectRemote(options: RemoteTarget) {
      return Promise.resolve({
        success: false,
        error: `Remote connect is not supported in this browser-only Web mode: ${options.kind}`,
      });
    },
    cancelPendingRemoteConnection: (_requestId?: string) => Promise.resolve(),
    disposeRemoteSession: () => Promise.resolve(),
    isDockerAvailable: () => Promise.resolve(false),
    listWSLDistros: () => Promise.resolve([]),
    listDockerContainers: () => Promise.resolve([]),
    listSSHConfigAliases: () => Promise.resolve([]),
    loadMcpFromUserDirectory: () => Promise.resolve({ servers: [] }),
    saveMcpToUserDirectory: () =>
      Promise.resolve({
        success: false,
        error: "MCP native directory management requires a desktop attachment",
      }),
    migrateLegacyCommonMcp: () =>
      Promise.resolve({
        servers: {},
        totalCount: 0,
        importedCount: 0,
        skippedCount: 0,
      }),
    openExternal: (url) => {
      window.open(url, "_blank", "noopener,noreferrer");
    },
    openFeedback: async () => {
      const feedbackUrl = await resolveFeedbackUrl();
      if (!feedbackUrl) {
        return;
      }
      window.open(feedbackUrl, "_blank", "noopener,noreferrer");
    },
    openCommunity: async () => {
      const locale = document.documentElement.lang === "en-US" ? "en-US" : "zh-CN";
      const communityUrl = await resolveWebCommunityUrl(locale);
      if (!communityUrl) {
        return;
      }
      window.open(communityUrl, "_blank", "noopener,noreferrer");
    },
    canOpenCommunity: async (locale) => {
      const communityUrl = await resolveWebCommunityUrl(locale);
      return typeof communityUrl === "string" && communityUrl.length > 0;
    },
    openInFileManager: () =>
      Promise.resolve({ success: false, error: "Not supported in web mode" }),
    openExternalFile: () => Promise.resolve({ success: false, error: "Not supported in web mode" }),
    registerOAuthState: (_payload) => {},
    onOAuthCallback: () => () => {},
    onPaymentCallback: () => () => {},
    onShareImport: () => () => {},
    notifyRendererReady: () => {},
    reportTelemetryEvent: async () => {},
    reportArmsCustomEvent: () => Promise.resolve(),
    showTaskNotification: (payload) => {
      if (document.hasFocus()) {
        return;
      }

      if (
        typeof window.Notification === "undefined" ||
        window.Notification.permission !== "granted"
      ) {
        return;
      }

      try {
        new window.Notification(payload.title, {
          body: payload.body,
          silent: true,
        });
        void playTaskNotificationSound();
      } catch {
        // æµè§ˆå™¨é€šçŸ¥ä¸å¯ç”¨æ—¶é™é»˜å¿½ç•¥ï¼Œé¿å…æ‰“æ–­ä¸»æµç¨‹
      }
    },
    // Web ç«¯ä¸éœ€è¦è·¨çª—å£ tab ç®¡ç†
    syncWindowTabs: () => {},
    // Web ç«¯æ²¡æœ‰å®¿ä¸»å±‚ Dock / ä»»åŠ¡æ å¾½æ ‡ï¼Œä¿æŒç©ºå®žçŽ°ä»¥å…¼å®¹ç»Ÿä¸€å¹³å°æŽ¥å£
    syncWindowUnreadCount: () => {},
    syncActiveTaskSession: () => {},
    onFocusTab: () => () => {},
    onNewTab: () => () => {},
    onCloseActiveContextRequest: () => () => {},
    onOpenBrowserUrl: () => () => {},
    onNewTask: () => () => {},
    onOpenWorkspace: () => () => {},
    onWindowFullscreenChanged: () => () => {},
    onTaskNotificationClick: () => () => {},
    exportLogs: () => Promise.resolve({ success: false, error: "Not supported in web mode" }),
    captureWindowScreenshot: () => Promise.resolve(null),
    importChromeBrowserData: (_options) =>
      Promise.resolve({
        success: false,
        cookies: { imported: 0, skipped: 0, failed: 0 },
        localStorage: {
          originsImported: 0,
          entriesImported: 0,
          originsSkipped: 0,
          originsFailed: 0,
        },
        error: "chrome_import_not_supported" as const,
      }),
    clearEmbeddedBrowserData: () =>
      Promise.resolve({ success: false, error: "Not supported in web mode" }),
    // IPlatformService æ–°å¢žæ›´æ–°æç¤ºèƒ½åŠ›åŽï¼ŒWeb fallback æ²¡æœ‰åŒæ­¥è¡¥é½ç©ºå®žçŽ°ï¼Œ
    // æ ¹çº§ typecheck ä¼šç›´æŽ¥å¤±è´¥ï¼Œè¿žä¸Žæ¡Œé¢ç«¯æ— å…³çš„æ”¹åŠ¨éƒ½æ²¡æ³•å®Œæˆæ ¡éªŒã€‚
    // Web ç«¯å½“å‰æ²¡æœ‰æ¡Œé¢æ›´æ–°å™¨ï¼Œå…ˆæ˜¾å¼ no-opï¼Œä¿æŒæŽ¥å£å®Œæ•´ä¸”ä¸æ”¹å˜çŽ°æœ‰è¡Œä¸ºã€‚
    onUpdateReady: () => () => {},
    onUpdateCheckResult: () => () => {},
    onUpdateStateChanged: () => () => {},
    getUpdateState: () => Promise.resolve({ kind: "idle", enabled: true }),
    downloadUpdate: () => Promise.resolve(),
    cancelUpdateDownload: () => Promise.resolve(),
    getDesktopSessionActivity: () => Promise.resolve({ runningAgentSessionCount: 0 }),
    getDesktopZoomLevel: () => Promise.resolve({ zoomLevel: 0 }),
    onDesktopZoomLevelChanged: () => () => {},
    onPostUpdateReleaseNotes: () => () => {},
    acknowledgePostUpdateReleaseNotes: () => Promise.resolve(),
    skipUpdateVersion: () => Promise.resolve(),
    quitAndInstallUpdate: () => Promise.resolve(),
    getInstalledEditors: () => Promise.resolve([]),
    openInEditor: () => Promise.resolve({ success: false, error: "Not supported in web mode" }),
    executeDesktopCommand: () => Promise.resolve(),
    setApplicationLocale: (_locale) => Promise.resolve(),
    setTitleBarTheme: () => Promise.resolve(),
    getDeviceId: () => {
      const nav = globalThis.navigator as Navigator & { platform?: string };
      const platform = nav?.platform ?? "";
      const screenWidth = globalThis.screen?.width;
      const screenHeight = globalThis.screen?.height;
      const colorDepth = globalThis.screen?.colorDepth;
      const parts = [
        platform,
        screenWidth !== undefined ? String(screenWidth) : "",
        screenHeight !== undefined ? String(screenHeight) : "",
        colorDepth !== undefined ? String(colorDepth) : "",
      ];
      return parts.filter(Boolean).join("|");
    },
  };
}

type TauriInvoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>;
interface TauriDragDropEventPayload {
  type?: string;
  paths?: string[];
  position?: { x: number; y: number };
}
interface TauriWebviewHandle {
  onDragDropEvent?: (
    handler: (event: { payload?: TauriDragDropEventPayload }) => void,
  ) => Promise<() => void>;
}
interface TauriGlobal {
  core?: { invoke?: TauriInvoke };
  webview?: { getCurrentWebview?: () => TauriWebviewHandle };
}
type LoadMcpFromUserDirectoryResult = Awaited<
  ReturnType<NonNullable<IPlatformService["loadMcpFromUserDirectory"]>>
>;
type MigrateLegacyCommonMcpResult = Awaited<
  ReturnType<NonNullable<IPlatformService["migrateLegacyCommonMcp"]>>
>;
type CreateTempTextAttachmentResult = Awaited<
  ReturnType<NonNullable<IPlatformService["createTempTextAttachment"]>>
>;
function tauriInvoke(): TauriInvoke | null {
  const tauri = (globalThis as { __TAURI__?: TauriGlobal }).__TAURI__;
  return tauri?.core?.invoke ?? null;
}

function tauriWebview(): TauriWebviewHandle | null {
  const tauri = (globalThis as { __TAURI__?: TauriGlobal }).__TAURI__;
  return tauri?.webview?.getCurrentWebview?.() ?? null;
}

// Desktop identity: the Rust shell persists `deviceMid` in
// `{appConfigDir}/telemetry-state.json` (shared with the CLI/telemetry). The
// value is primed before the first render so the synchronous `getDeviceId()`
// contract can return it; the web fingerprint stays as the explicit fallback.
let tauriDeviceId: string | null = null;
let tauriDeviceIdPending: Promise<string> | null = null;
let browserUseLifecycleBridgeInstalled = false;

function primeTauriDeviceId(): Promise<string> {
  if (tauriDeviceIdPending) return tauriDeviceIdPending;
  const invoke = tauriInvoke();
  tauriDeviceIdPending = (invoke
    ? (invoke("get_device_id") as Promise<string>)
    : Promise.resolve(generateMobileDeviceFingerprint())
  )
    .then((value) => {
      const normalized = typeof value === "string" ? value.trim() : "";
      tauriDeviceId = normalized.length > 0 ? normalized : generateMobileDeviceFingerprint();
      return tauriDeviceId;
    })
    .catch(() => {
      tauriDeviceId = generateMobileDeviceFingerprint();
      return tauriDeviceId;
    });
  return tauriDeviceIdPending;
}

/**
 * Tauri platform adapter: the web build is also the Tauri desktop renderer, so
 * only the native-capability subset is overridden. Everything else keeps the
 * explicit web fallback (unsupported, never a fake success).
 */
function createTauriPlatform(): IPlatformService {
  const invoke = tauriInvoke();
  if (!invoke) return createWebPlatform();
  const base = createWebPlatform();
  // Remote-workspace transport: same-origin calls into the embedded Rust server.
  const remoteClient = createRemoteWorkspaceClient(
    (input, init) => fetch(input, init),
    window.location.origin,
  );
  const oauthStates = new Set<string>();
  const deepLinkListeners = new Set<(url: URL) => void>();
  const normalizeDeepLinkPath = (pathname: string): string => {
    const withoutTrailingSlash = pathname.replace(/\/+$/, "");
    const normalized = withoutTrailingSlash === "" ? "/" : withoutTrailingSlash;
    return `/${normalized.replace(/^\/+/, "")}`;
  };
  const isOAuthCallback = (url: URL): boolean => {
    if (url.protocol !== "zcode:") return false;
    const path = normalizeDeepLinkPath(url.pathname);
    if (url.hostname === "oauth") return path === "/callback";
    if (url.hostname) return false;
    const [, host, ...rest] = path.split("/");
    return host === "oauth" && `/${rest.join("/")}` === "/callback";
  };
  const isPaymentCallback = (url: URL): boolean => {
    if (url.protocol !== "zcode:") return false;
    const path = normalizeDeepLinkPath(url.pathname);
    if (url.hostname === "payment") return path === "/callback";
    if (url.hostname) return false;
    const [, host, ...rest] = path.split("/");
    return host === "payment" && `/${rest.join("/")}` === "/callback";
  };
  const isShareImport = (url: URL): boolean =>
    url.protocol === "zcode:" &&
    url.hostname === "share" &&
    normalizeDeepLinkPath(url.pathname) === "/import";
  const shareImportCode = (url: URL): string | null => {
    if (!isShareImport(url)) return null;
    const code = url.searchParams.get("code")?.trim();
    return code && /^[A-Za-z0-9._~-]{1,512}$/u.test(code) ? code : null;
  };
  const dispatchDeepLink = (raw: string): void => {
    try {
      const url = new URL(raw);
      for (const listener of deepLinkListeners) listener(url);
    } catch {
      // A malformed deep link must not break the app.
    }
  };
  const listenDeepLink = (callback: (url: URL) => void): (() => void) => {
    deepLinkListeners.add(callback);
    const event = (globalThis as {
      __TAURI__?: {
        event?: {
          listen?: (
            name: string,
            handler: (event: { payload: string }) => void,
          ) => Promise<() => void>;
        };
      };
    }).__TAURI__?.event;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    if (event?.listen) {
      void event
        .listen("zcode:deep-link", (payload) => {
          if (!disposed) dispatchDeepLink(payload.payload);
        })
        .then((dispose) => {
          if (disposed) dispose();
          else unlisten = dispose;
        });
    }
    return () => {
      disposed = true;
      deepLinkListeners.delete(callback);
      unlisten?.();
    };
  };
  const listenDesktopCommand = (
    command: string,
    callback: () => void,
  ): (() => void) => {
    const event = (globalThis as {
      __TAURI__?: {
        event?: {
          listen?: (
            name: string,
            handler: (event: { payload: string }) => void,
          ) => Promise<() => void>;
        };
      };
    }).__TAURI__?.event;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    if (event?.listen) {
      void event
        .listen("zcode:desktop-command", (payload) => {
          if (!disposed && payload.payload === command) callback();
        })
        .then((dispose) => {
          if (disposed) dispose();
          else unlisten = dispose;
        });
    }
    return () => {
      disposed = true;
      unlisten?.();
    };
  };
  const listenTauriEvent = <T,>(
    name: string,
    handler: (payload: T) => void,
  ): (() => void) => {
    const event = (globalThis as {
      __TAURI__?: {
        event?: {
          listen?: (
            name: string,
            handler: (event: { payload: T }) => void,
          ) => Promise<() => void>;
        };
      };
    }).__TAURI__?.event;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    if (event?.listen) {
      void event
        .listen(name, (payload) => {
          if (!disposed) handler(payload.payload);
        })
        .then((dispose) => {
          if (disposed) dispose();
          else unlisten = dispose;
        });
    }
    return () => {
      disposed = true;
      unlisten?.();
    };
  };
  // Renderer-owned browser-use tab lifecycle: the Rust host emits a request
  // event; the controller answers through `browser_use_lifecycle_result`.
  if (!browserUseLifecycleBridgeInstalled) {
    browserUseLifecycleBridgeInstalled = true;
    listenTauriEvent<{
      requestId?: unknown;
      command?: {
        method?: unknown;
        tabId?: unknown;
        keep?: unknown;
        deliverable?: unknown;
        name?: unknown;
        recordingId?: unknown;
        outputPath?: unknown;
        options?: unknown;
      };
    }>("zcode:browser-use-lifecycle-request", (payload) => {
      const requestId = typeof payload?.requestId === "string" ? payload.requestId : "";
      if (!requestId) return;
      const method = typeof payload?.command?.method === "string" ? payload.command.method : "";
      const tabId = typeof payload?.command?.tabId === "string" ? payload.command.tabId : "";
      const rawKeep = payload?.command?.keep;
      let keep: Array<{ tabId: string; status: "handoff" | "deliverable" }> | null = null;
      if (Array.isArray(rawKeep)) {
        const parsed: Array<{ tabId: string; status: "handoff" | "deliverable" }> =
          rawKeep.flatMap((item) => {
            if (!item || typeof item !== "object") return [];
            const record = item as { tabId?: unknown; status?: unknown };
            const status = record.status;
            if (typeof record.tabId !== "string" || !record.tabId) return [];
            if (status !== "handoff" && status !== "deliverable") return [];
            return [{ tabId: record.tabId, status }];
          });
        if (parsed.length === rawKeep.length) keep = parsed;
      }
      const outcomeResult = (outcome: {
        ok: boolean;
        tab?: unknown;
        recording?: unknown;
        code?: string;
        message?: string;
      }): Record<string, unknown> =>
        outcome.ok
          ? {
              ok: true,
              ...(outcome.tab ? { tab: outcome.tab } : {}),
              ...(outcome.recording ? { recording: outcome.recording } : {}),
            }
          : { ok: false, error: { code: outcome.code, message: outcome.message } };
      void (async () => {
        let result: Record<string, unknown>;
        const controller = getBrowserUseLifecycleController();
        if (!controller) {
          // Welcome/login screen: no workspace shell has registered a
          // controller yet, so the truthful answer is "no browser tabs".
          result =
            method === "listUserTabs"
              ? {
                  ok: true,
                  userTabs: [],
                  meta: {
                    browserUse: true,
                    backendType: "iab",
                    browserId: "iab:renderer",
                    browserGeneration: 1,
                    openTabIds: [],
                  },
                }
              : {
                  ok: false,
                  error: {
                    code: "backend_unavailable",
                    message: "no active browser workspace",
                  },
                };
        } else if (method === "listUserTabs") {
          const tabs = controller.listUserTabs();
          result = {
            ok: true,
            userTabs: tabs.map((tab) => ({
              id: tab.tabId,
              url: tab.url,
              title: tab.title,
            })),
            meta: {
              browserUse: true,
              backendType: "iab",
              browserId: "iab:renderer",
              browserGeneration: 1,
              openTabIds: tabs.map((tab) => tab.tabId),
            },
          };
        } else if (method === "activateTab") {
          result =
            tabId && controller.activateTab(tabId)
              ? {
                  ok: true,
                  meta: {
                    browserUse: true,
                    backendType: "iab",
                    browserId: "iab:renderer",
                    browserGeneration: 1,
                    openTabIds: [tabId],
                    tabId,
                  },
                }
              : {
                  ok: false,
                  error: { code: "ref_not_found", message: "browser tab not found" },
                };
        } else if (method === "close") {
          const closed = tabId ? await controller.close(tabId) : false;
          result = closed
            ? { ok: true }
            : { ok: false, error: { code: "ref_not_found", message: "browser tab not found" } };
        } else if (method === "newTab") {
          result = outcomeResult(controller.newTab());
        } else if (method === "finalize") {
          const deliverable = payload?.command?.deliverable;
          result = outcomeResult(
            controller.finalize({
              ...(tabId ? { tabId } : {}),
              ...(typeof deliverable === "boolean" ? { deliverable } : {}),
            }),
          );
        } else if (method === "nameSession") {
          const name = typeof payload?.command?.name === "string" ? payload.command.name : "";
          result = name
            ? outcomeResult(controller.nameSession(name))
            : {
                ok: false,
                error: { code: "invalid_input", message: "nameSession requires `name`" },
              };
        } else if (method === "closeSession") {
          result = outcomeResult(controller.closeSession());
        } else if (method === "recordingStart") {
          const rawOptions = payload?.command?.options;
          const options =
            rawOptions && typeof rawOptions === "object"
              ? (rawOptions as BrowserRecordingOptions)
              : {};
          result = outcomeResult(
            controller.recordingStart({
              ...options,
              ...(tabId ? { tabId } : {}),
            }),
          );
        } else if (method === "recordingStatus" || method === "recordingCancel") {
          const recordingId =
            typeof payload?.command?.recordingId === "string"
              ? payload.command.recordingId
              : "";
          if (!recordingId) {
            result = {
              ok: false,
              error: {
                code: "invalid_input",
                message: `${method} requires \`recordingId\``,
              },
            };
          } else if (method === "recordingStatus") {
            const outputPath =
              typeof payload?.command?.outputPath === "string"
                ? payload.command.outputPath
                : undefined;
            result = outcomeResult(
              controller.recordingStatus(recordingId, {
                ...(tabId ? { tabId } : {}),
                ...(outputPath ? { outputPath } : {}),
              }),
            );
          } else {
            result = outcomeResult(
              controller.recordingCancel(recordingId, tabId ? { tabId } : {}),
            );
          }
        } else if (method === "claimTab") {
          result = tabId
            ? outcomeResult(controller.claimTab(tabId))
            : {
                ok: false,
                error: { code: "invalid_input", message: "claimTab requires `tabId`" },
              };
        } else if (method === "finalizeTabs") {
          result =
            keep === null
              ? {
                  ok: false,
                  error: {
                    code: "invalid_input",
                    message: "finalizeTabs requires `keep: [{tabId, status}]`",
                  },
                }
              : outcomeResult(controller.finalizeTabs(keep));
        } else if (method === "markDeliverable" || method === "markHandoff") {
          const outcome = tabId
            ? method === "markDeliverable"
              ? controller.markDeliverable(tabId)
              : controller.markHandoff(tabId)
            : null;
          result = outcome
            ? outcomeResult(outcome)
            : {
                ok: false,
                error: { code: "invalid_input", message: `${method} requires \`tabId\`` },
              };
        } else {
          result = {
            ok: false,
            error: {
              code: "capability_unsupported",
              message: `lifecycle method ${method || "(missing)"} is not supported`,
            },
          };
        }
        try {
          await invoke("browser_use_lifecycle_result", { requestId, result });
        } catch (error) {
          console.warn("[browser-use] lifecycle response failed", error);
        }
      })();
    });
  }
  return {
    ...base,
    canSelectFilePath: true,
    selectDirectory: () => invoke("select_directory") as Promise<string | null>,
    selectFile: () => invoke("select_file") as Promise<string | null>,
    selectFiles: () => invoke("select_files") as Promise<string[]>,
    saveFile: async (payload) => {
      const request = payload.data
        ? {
            suggestedName: payload.suggestedName,
            data: Array.from(new Uint8Array(payload.data)),
          }
        : {
            suggestedName: payload.suggestedName,
            sourceUrl: payload.sourceUrl,
          };
      return invoke("save_file", { payload: request }) as Promise<SaveFileResult>;
    },
    browserViewSetOccluded: (payload: {
      key?: string;
      tabId?: string;
      occluded: boolean;
    }) =>
      invoke("browser_view_set_occluded", {
        key: payload.key ?? payload.tabId ?? "",
        occluded: payload.occluded,
      }),
    browserViewCdp: async (payload: {
      key: string;
      method: string;
      params?: Record<string, unknown>;
    }) => {
      const raw = await invoke("browser_view_cdp", {
        key: payload.key,
        method: payload.method,
        params: JSON.stringify(payload.params ?? {}),
      });
      return typeof raw === "string" ? JSON.parse(raw) : raw;
    },
    saveBrowserRecording: (payload: {
      workspacePath: string;
      relativePath: string;
      data: ArrayBuffer;
    }) =>
      invoke("save_browser_recording", {
        workspacePath: payload.workspacePath,
        relativePath: payload.relativePath,
        data: Array.from(new Uint8Array(payload.data)),
      }) as Promise<string>,
    printPageToPdf: async () => {
      const result = (await invoke("print_page_to_pdf")) as {
        success?: boolean;
        data?: number[];
        error?: string;
      };
      if (result.success && Array.isArray(result.data)) {
        return { success: true, data: new Uint8Array(result.data).buffer };
      }
      return { success: false, error: result.error ?? "print_failed" };
    },
    registerOAuthState: (payload) => {
      oauthStates.add(payload.state);
    },
    onOAuthCallback: (callback) =>
      listenDeepLink((url) => {
        if (!isOAuthCallback(url)) return;
        const state = url.searchParams.get("state");
        if (!state || !oauthStates.has(state)) return;
        if (url.searchParams.has("code") || url.searchParams.has("authCode")) {
          oauthStates.delete(state);
        }
        callback(url.toString());
      }),
    onPaymentCallback: (callback) =>
      listenDeepLink((url) => {
        if (isPaymentCallback(url)) callback(url.toString());
      }),
    onShareImport: (callback) =>
      listenDeepLink((url) => {
        const shareCode = shareImportCode(url);
        if (shareCode) callback({ shareCode });
      }),
    notifyRendererReady: () => {
      void invoke("deep_link_current").then((raw) => {
        if (typeof raw === "string") dispatchDeepLink(raw);
      });
    },
    onNewTask: (handler) => listenDesktopCommand("newTask", handler),
    onOpenWorkspace: (handler) => listenDesktopCommand("openWorkspace", handler),
    onFocusTab: (handler) =>
      listenTauriEvent<string>("zcode:focus-tab", (path) => {
        if (typeof path === "string" && path.trim().length > 0) handler(path);
      }),
    onNewTab: (handler) => listenTauriEvent<unknown>("zcode:new-tab", () => handler()),
    onTaskNotificationClick: (handler) =>
      listenTauriEvent<string>("zcode:task-notification-click", (taskId) => {
        if (typeof taskId === "string" && taskId.trim().length > 0) handler(taskId);
      }),
    syncWindowTabs: (paths) => {
      void invoke("sync_window_tabs", { paths });
    },
    syncWindowUnreadCount: (count) => {
      void invoke("sync_window_unread_count", { count });
    },
    syncActiveTaskSession: (sessionId) => {
      void invoke("sync_active_task_session", { sessionId });
    },
    syncAppSettings: (patch) => {
      void invoke("sync_app_settings", { patch });
    },
    openExternal: (url) => {
      void invoke("open_external_url", { url });
    },
    // The web implementation resolves the same config but uses `window.open`,
    // which opens an embedded webview in Tauri; route it through the Rust
    // opener so feedback/community land in the system browser.
    openFeedback: async () => {
      const feedbackUrl = await resolveFeedbackUrl();
      if (feedbackUrl) await invoke("open_external_url", { url: feedbackUrl });
    },
    openCommunity: async () => {
      const locale = document.documentElement.lang === "en-US" ? "en-US" : "zh-CN";
      const communityUrl = await resolveWebCommunityUrl(locale);
      if (communityUrl) await invoke("open_external_url", { url: communityUrl });
    },
    getDeviceId: () => tauriDeviceId ?? generateMobileDeviceFingerprint(),
    // The OS language comes from the shell (Electron `resolveSystemApplicationLocale`
    // semantics), not from `navigator.language`, which a WebView may report differently.
    getSystemLocale: () => invoke("system_locale") as Promise<Locale>,
    onWindowFullscreenChanged: (handler) =>
      listenTauriEvent<boolean>("zcode:window-fullscreen", (payload) => handler(payload)),
    onFileDropPaths: (handler) => {
      const webview = tauriWebview();
      if (!webview?.onDragDropEvent) return () => {};
      let disposed = false;
      let unlisten: (() => void) | undefined;
      void webview
        .onDragDropEvent((event) => {
          const payload = event?.payload;
          if (!payload || payload.type !== "drop") return;
          const paths = (payload.paths ?? []).filter(
            (path): path is string => typeof path === "string" && path.trim().length > 0,
          );
          if (paths.length === 0) return;
          const position =
            payload.position &&
            typeof payload.position.x === "number" &&
            typeof payload.position.y === "number"
              ? { x: payload.position.x, y: payload.position.y }
              : undefined;
          handler({ paths, position });
        })
        .then((dispose) => {
          if (disposed) dispose();
          else unlisten = dispose;
        })
        .catch(() => {
          // The subscription is optional; DOM File drops keep working without it.
        });
      return () => {
        disposed = true;
        unlisten?.();
      };
    },
    openExternalFile: async (path) => {
      try {
        await invoke("open_path", { path });
        return { success: true };
      } catch (error) {
        return { success: false, error: String(error) };
      }
    },
    openInFileManager: async (path) => {
      try {
        // The shell selects the file in Explorer/Finder (`reveal_path`); unlike
        // `open_path` this does not launch the file's default application.
        await invoke("reveal_path", { path });
        return { success: true };
      } catch (error) {
        return { success: false, error: String(error) };
      }
    },
    // Remote sessions are owned by the embedded Rust server's registry
    // (`/api/remote/*`). The renderer talks to it directly; a failure reports the
    // server's own reason instead of pretending a connection was established.
    connectRemote: (options, requestId, context) =>
      remoteClient.connectRemote(
        options,
        requestId,
        context ?? { workspacePath: "" },
      ),
    cancelPendingRemoteConnection: (requestId) =>
      remoteClient.cancelPendingRemoteConnection(requestId),
    bindRemoteWorkspaceSessionContext: (context) =>
      remoteClient.bindRemoteWorkspaceSessionContext(context),
    disposeRemoteSession: (sessionId) => remoteClient.disposeRemoteSession(sessionId),
    isDockerAvailable: () => invoke("is_docker_available") as Promise<boolean>,
    listWSLDistros: () => invoke("list_wsl_distros") as Promise<WSLDistro[]>,
    listDockerContainers: () =>
      invoke("list_docker_containers") as Promise<DockerContainerInfo[]>,
    listSSHConfigAliases: () =>
      invoke("list_ssh_config_aliases") as Promise<SSHConfigAliasOption[]>,
    browserViewAttachGuest: async (payload) => {
      const result = (await invoke("browser_view_attach_guest", {
        key: payload.key,
        webContentsId: payload.webContentsId,
      })) as {
        ok?: boolean;
        guestGeneration?: number;
        reason?: string;
        recoveryRequested?: boolean;
      };
      if (result?.ok) {
        return { ok: true, guestGeneration: result.guestGeneration ?? 1 };
      }
      return {
        ok: false,
        reason: (result?.reason as BrowserGuestAttachRejectReason | undefined) ?? "not-webview",
        recoveryRequested: Boolean(result?.recoveryRequested),
      };
    },
    browserViewDetachGuest: (payload) =>
      invoke("browser_view_detach_guest", {
        key: payload.key,
        webContentsId: payload.webContentsId,
      }) as Promise<boolean>,
    browserViewCloseTab: async (payload) => {
      await invoke("browser_view_close", { key: payload.tabId });
    },
    importChromeBrowserData: (options) =>
      invoke("import_chrome_browser_data", {
        options: options ?? {},
      }) as Promise<ChromeBrowserDataImportResult>,
    exportLogs: () =>
      invoke("export_logs") as Promise<{
        success: boolean;
        path?: string;
        error?: string;
      }>,
    captureWindowScreenshot: () =>
      invoke("capture_window_screenshot") as Promise<{
        dataBase64: string;
        filename: string;
        contentType: string;
        size: number;
      } | null>,
    getInstalledEditors: () =>
      invoke("get_installed_editors") as Promise<
        Array<{ id: string; name: string; iconDataUrl: string }>
      >,
    openInEditor: (editorId, path, options) =>
      invoke("open_in_editor", {
        editorId,
        path,
        options: options ?? {},
      }) as Promise<{ success: boolean; error?: string }>,
    loadMcpFromUserDirectory: (payload) =>
      invoke("load_mcp_from_user_directory", {
        request: payload ?? {},
      }) as Promise<LoadMcpFromUserDirectoryResult>,
    saveMcpToUserDirectory: (payload) =>
      invoke("save_mcp_to_user_directory", { payload }) as Promise<{
        success: boolean;
        error?: string;
      }>,
    migrateLegacyCommonMcp: (payload) =>
      invoke("migrate_legacy_common_mcp", {
        request: payload ?? {},
      }) as Promise<MigrateLegacyCommonMcpResult>,
    createTempTextAttachment: (payload) =>
      invoke("create_temp_text_attachment", {
        request: payload,
      }) as Promise<CreateTempTextAttachmentResult>,
    showTaskNotification: (payload) => {
      void invoke("show_task_notification", {
        taskId: payload.taskId,
        title: payload.title,
        body: payload.body,
      });
    },
    // Renderer telemetry is exported through the Rust OTLP/HTTP sink; when no
    // OTLP endpoint is configured the shell drops the events.
    reportTelemetryEvent: (payload) =>
      invoke("report_telemetry_event", { payload }) as Promise<void>,
    reportArmsCustomEvent: (payload) =>
      invoke("report_arms_custom_event", { payload }) as Promise<void>,
    reportRendererActionTraceBatch: (batch) => {
      void invoke("report_renderer_action_trace_batch", { batch });
    },
    reportRendererHeapSample: (sample) => {
      void invoke("report_renderer_heap_sample", { sample });
    },
    reportLocalTtftBatch: (batch) => {
      void invoke("report_local_ttft_batch", { batch });
    },
    executeDesktopCommand: (command) => invoke("desktop_command", { command }),
    getDesktopZoomLevel: () =>
      invoke("get_desktop_zoom_level") as Promise<{ zoomLevel: number }>,
    onDesktopZoomLevelChanged: (handler) =>
      listenTauriEvent<{ zoomLevel: number }>("zcode:desktop-zoom-changed", handler),
    getDesktopWindowChromeState: async () => {
      const state = (await invoke("get_desktop_window_chrome_state")) as {
        isMaximized?: boolean;
      };
      return {
        isMaximized: Boolean(state?.isMaximized),
        supportsNativeRoundedCorners: false,
      };
    },
    onUpdateStateChanged: (handler) =>
      listenTauriEvent<unknown>("zcode:update-state-changed", handler as (payload: unknown) => void),
    onUpdateCheckResult: (handler) =>
      listenTauriEvent<unknown>("zcode:update-check-result", handler as (payload: unknown) => void),
    onUpdateReady: (handler) =>
      listenTauriEvent<unknown>("zcode:update-ready", handler as (payload: unknown) => void),
    getUpdateState: () => invoke("get_update_state") as Promise<any>,
    checkForUpdates: () => invoke("check_for_updates") as Promise<any>,
    downloadUpdate: () => invoke("download_update") as Promise<void>,
    cancelUpdateDownload: () => invoke("cancel_update_download") as Promise<void>,
    quitAndInstallUpdate: () => invoke("quit_and_install_update") as Promise<void>,
    skipUpdateVersion: (version: string) => invoke("skip_update_version", { version }),
    acknowledgePostUpdateReleaseNotes: () => invoke("acknowledge_post_update_release_notes") as Promise<void>,
    onPostUpdateReleaseNotes: (handler) =>
      listenTauriEvent<unknown>("zcode:post-update-release-notes", handler as (payload: unknown) => void),
  } as IPlatformService;
}

function resolveDefaultWsOrigin(): string {
  return `${window.location.protocol === "https:" ? "wss:" : "ws:"}//${window.location.host}`;
}

async function resolveWebBootstrap(): Promise<WebBootstrapResult> {
  const params = new URLSearchParams(window.location.search);
  const remoteId = params.get("remote");
  const wsUrl = remoteId
    ? `${resolveDefaultWsOrigin()}/ws/remote/${remoteId}`
    : `${resolveDefaultWsOrigin()}/ws`;

  if (remoteId) {
    return { wsUrl };
  }

  try {
    const response = await fetch("/api/server-info", {
      cache: "no-store",
    });
    if (!response.ok) {
      return { wsUrl };
    }
    const serverInfo = (await response.json()) as Partial<ServerRemoteInfo>;
    const workspace = Array.isArray(serverInfo.workspaces) ? serverInfo.workspaces[0] : undefined;
    return {
      wsUrl,
      ...(workspace?.path ? { initialWorkspaceAbsPath: workspace.path } : {}),
      ...(workspace?.workspaceIdentity
        ? { initialWorkspaceIdentity: workspace.workspaceIdentity }
        : {}),
    };
  } catch {
    return { wsUrl };
  }
}

function WebBootstrapErrorScreen({ message }: { message: string }) {
  return (
    <div className="h-dvh min-h-dvh w-screen bg-background text-foreground">
      <div className="mx-auto flex h-full w-full max-w-lg items-center px-4">
        <section className="w-full rounded-xl border border-card-border bg-card p-5">
          <div className="flex items-center gap-3">
            <span className="size-2 rounded-full bg-destructive" />
            <h1 className="text-ui-xs font-medium">
              {/^zh\b/i.test(navigator.language) ? "Web å¯åŠ¨å¤±è´¥" : "Web bootstrap failed"}
            </h1>
          </div>
          <p className="mt-2 break-all text-ui-xs/relaxed text-foreground-subtle">{message}</p>
          <button
            type="button"
            className="mt-4 rounded-lg border border-border bg-surface px-3 py-2 text-ui-xs text-foreground-subtle hover:bg-surface-hover"
            onClick={() => {
              window.location.reload();
            }}
          >
            {/^zh\b/i.test(navigator.language) ? "é‡è¯•" : "Retry"}
          </button>
        </section>
      </div>
    </div>
  );
}

export function isNcagPath(pathname: string): boolean {
  const normalized = pathname.replace(/\/+$/, "");
  return normalized === "/nc-ag" || normalized === "/cn/nc-ag";
}

function renderWebBootstrapError(error: unknown): void {
  document.title = "ZCode - Web";
  root.render(
    <WebBootstrapErrorScreen message={error instanceof Error ? error.message : String(error)} />,
  );
}

async function bootstrapWebApp() {
  const params = new URLSearchParams(window.location.search);
  if (isWebOAuthCallback(params)) {
    renderWebAuthCallbackPage();
    return;
  }

  if (isConversationSharePath(window.location.pathname)) {
    await renderConversationSharePage();
    return;
  }

  // NC-AG operational dashboard (`/nc-ag`): a real page served by the same backend.
  if (isNcagPath(window.location.pathname)) {
    document.title = "NC-AG Orchestrator";
    root.render(
      <AppErrorBoundary>
        <NcagApp />
      </AppErrorBoundary>,
    );
    return;
  }

  let bootstrap: WebBootstrapResult;
  try {
    bootstrap = await resolveWebBootstrap();
  } catch (error) {
    renderWebBootstrapError(error);
    return;
  }

  try {
    const services = await connectViaWebSocket(bootstrap.wsUrl, {
      onClose: () => {},
    });
    const isTauri = tauriInvoke() !== null;
    const platform = isTauri ? createTauriPlatform() : createWebPlatform();
    if (isTauri) setStreamClientId(await primeTauriDeviceId());
    document.title = isTauri ? "ZCode" : "ZCode - Web + Server";

    root.render(
      <AppErrorBoundary>
        <ZCodeIntlProvider
          settingService={services.settingService}
          broadcastService={services.broadcastService}
        >
          <Root
            services={services}
            platform={platform}
            initialWorkspaceAbsPath={bootstrap.initialWorkspaceAbsPath}
            initialWorkspaceIdentity={bootstrap.initialWorkspaceIdentity}
            initialTaskId={bootstrap.initialTaskId}
            restoreSession={bootstrap.restoreSession}
            allowOpenWorkspace={bootstrap.allowOpenWorkspace}
            preferDirectoryBrowser
            supportsEmbeddedBrowser={false}
            allowRemoteWorkspace={false}
          />
        </ZCodeIntlProvider>
      </AppErrorBoundary>,
    );
  } catch (error) {
    renderWebBootstrapError(error);
  }
}

void bootstrapWebApp();
