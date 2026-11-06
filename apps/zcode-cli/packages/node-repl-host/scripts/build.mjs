import { mkdir, readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { build } from "esbuild";

const packageRoot = resolve(import.meta.dirname, "..");

// See the fix of the same name in browser-use-plugin/scripts/build.mjs: esm product of esbuild
// __require shim has no require available in the ESM scope, the CJS dependency dragged in by @zcode/core (yaml →
// require("process")) will throw an error during the **module evaluation phase**, and the await import() of the plugin host will fail directly.
// The performance is that 0 tools are registered and mcp__node_repl__js is not visible at all on the model side. Inject true createRequire.
const nodeRequireBanner = `import { createRequire as __zcodeCreateRequire } from "node:module";
const require = __zcodeCreateRequire(import.meta.url);`;

// Officially wrap up Computer Use Zengquan
// Not available - each CUA call either waits until the MCP client times out (measured 60/110/120s) or returns after ~64s
// 「permission broker socket is not accepting connections yet」。
//
// Link: Lazy start (`lazy start` comment in services/src/node.ts) put darwin on Helper
//Installation/pull is moved from the host to "SDK self-pull when calling CUA for the first time", and that path runs in **this package**——
// `helperInstaller` therefore enters the official package with this bundle. But `__ZCODE_CUA_HELPER_BUILD_ID__` was previously
// **Only** packages/desktop/tsup.config.ts injection (Electron main/app.asar), this file
// esbuild doesn't call a single define. So `ZCODE_CUA_HELPER_BUILD_ID` of producer in the official package
// Fold into an empty string (see the bottom line of zcode-cua/src/broker/shared/cua-version.ts),
// `resolveExpectedCuaHelperBuildId()` returns null, helperInstaller hits the fail-closed guard:
//
//   "Packaged ZCode is missing its embedded Computer Use Helper build identity;
//    refusing an unpinned Helper install"
//
// As a result, the Helper neither installed nor started, and did not write a single line of logs. The throw statement was translated into a timeout by the MCP layer, and there was no inventory point——
// So it remains invisible. Empirical evidence: `[cua-product-helper]` in the host log is on 09-11 (host path, with define)
// There are 5 lines, 09-14 is 0 lines; `~/.zcode/computer-use/logs/` has never been created; after renaming the installed Helper
// Will not reinstall. dev is not affected (ALLOW_UNSIGNED_LOCAL + non-production runtime bypasses this guard),
// So it is only exposed in the official package and cannot be measured locally.
//
//Keep the value from the same source as the desktop side: CI injects ZCODE_CUA_HELPER_BUILD_ID env; dev is an empty string to avoid
// (dev Helper does not perform download/pin verification). See the comments for the define of the same name in packages/desktop/tsup.config.ts.
const resolveCuaHelperBuildId = (env = process.env) =>
  env.ZCODE_CUA_HELPER_BUILD_ID?.trim() ?? "";

export const buildNodeReplHostBundle = async ({
  outfile = resolve(packageRoot, "dist", "mcp", "server.js"),
  cuaHelperBuildId = resolveCuaHelperBuildId(),
} = {}) => {
  await mkdir(dirname(outfile), { recursive: true });
  await build({
    banner: { js: nodeRequireBanner },
    bundle: true,
    define: {
      __ZCODE_CUA_HELPER_BUILD_ID__: JSON.stringify(cuaHelperBuildId),
    },
    entryPoints: [resolve(packageRoot, "src", "server.ts")],
    format: "esm",
    legalComments: "none",
    outfile,
    platform: "node",
    target: "node24",
  });
// Guard during construction: Once the define name drifts (renamed, swallowed by refactoring such as createSharedDefines),
// The product will silently return an empty string, and the symptom only appears in the official package and manifests as a timeout. Fail immediately here, don't let it slip to the user.
  if (cuaHelperBuildId) {
    const bundled = await readFile(outfile, "utf8");
    if (!bundled.includes(cuaHelperBuildId)) {
      throw new Error(
        `[node-repl-host] ZCODE_CUA_HELPER_BUILD_ID=${cuaHelperBuildId} not collapsed into ${outfile}:` +
          "__ZCODE_CUA_HELPER_BUILD_ID__ define does not take effect, and the Helper installation of the official package will be rejected by fail-closed.",
      );
    }
  }
  return { outfile, cuaHelperBuildId };
};

// This was originally written as `file://${process.argv[1]}`.
// On Windows argv[1] is `C:\...\build.mjs`, and import.meta.url is `file:///C:/.../build.mjs`,
// The two are never equal - the script is imported as a pure module and exits without doing anything: the build "succeeds" but has no product,
// It is not exposed until the dev guard reports "build succeeded without required MCP runtime".
// The browser-use script with the same name and other entries in the warehouse use pathToFileURL. I missed this when extracting the package.
const entryPath = process.argv[1];
if (entryPath && import.meta.url === pathToFileURL(entryPath).href) {
  const { outfile } = await buildNodeReplHostBundle();
  console.log(`[node-repl-host] ${outfile}`);
}
