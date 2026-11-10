import {
  DEFAULT_ZCODE_ENDPOINT_ORIGIN,
  ZCODE_VERSION,
  buildHelpAppConfigUrl,
  createHelpAppConfigReader,
  resolveHelpAppConfig,
  type Locale,
} from "@zcode/shared";
import localDefaultAppConfig from "../../../config/default.json" with { type: "json" };

interface ResolveWebCommunityUrlOptions {
  fetchImpl?: typeof fetch;
  localConfig?: unknown;
  endpointOrigin?: string;
}

const readHelpConfig = createHelpAppConfigReader({
  fetchImpl: (input, init) => fetch(input, init),
});

export async function resolveWebHelpConfig(options: ResolveWebCommunityUrlOptions = {}) {
  const env = import.meta.env;
  const endpoint =
    options.endpointOrigin ??
    (env?.VITE_ZCODE_BASE_URL?.trim() ||
      env?.VITE_ZCODE_ENDPOINT_ORIGIN?.trim() ||
      DEFAULT_ZCODE_ENDPOINT_ORIGIN);
  // 服务端拒绝 platform=web；浏览器省略可选平台参数，避免伪装桌面系统。
  const url = buildHelpAppConfigUrl(endpoint, ZCODE_VERSION);
  // Tauri loads the same web build from the loopback origin; the remote config endpoint
  // has no CORS allowance for it, so the browser logs a failed request on every boot.
  // The embedded Rust shell already serves local Help config; skip the remote hop there.
  const isTauri = Boolean((globalThis as { __TAURI__?: unknown }).__TAURI__);
  let remote: unknown;
  if (!isTauri) {
    try {
      remote = await (
        options.fetchImpl
          ? createHelpAppConfigReader({ fetchImpl: options.fetchImpl })
          : readHelpConfig
      )(url);
    } catch {
      // 远端不可用时保留内置入口，不使用旧 CDN 作为第二个远端配置源。
    }
  }
  return resolveHelpAppConfig(remote, options.localConfig ?? localDefaultAppConfig);
}

export async function resolveWebCommunityUrl(
  locale: Locale,
  options: ResolveWebCommunityUrlOptions = {},
): Promise<string | undefined> {
  return (await resolveWebHelpConfig(options)).community_urls?.[locale];
}
