import { chmod, mkdir } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { build } from "esbuild";

const defaultPackageRoot = resolve(import.meta.dirname, "..");
const executableFileMode = 0o755;

// When esbuild is packaged with format: "esm", require() in the CJS dependency will be replaced with a __require shim:
//   typeof require !== "undefined" ? require : (name) => { throw Error('Dynamic require of "' + name + '" is not supported') }
// There is no require in the ESM module scope, so this shim will always go to the wrong branch.
// @zcode/core eagerly imported CJS from tool/handlers/write.js -> memory/origin-session.js
// yaml, yaml internal require("process") happens to hit shim, causing dist/mcp/server.js to be in the **module evaluation phase**
// Just throw `Dynamic require of "process" is not supported`; plugin host's await import() fails directly.
// Appears as mcp.server.closed / mcp.server.failed, 0 tools are registered, and mcp__node_repl__js is completely invisible on the model side.
//Inject the real createRequire here and let the shim fall on the available require (the product is still ESM).
// Add to both bundles: browser-client currently does not have CJS dependencies, but it is also an ESM product and will be dragged into one later.
// CJS dependencies will blow up at load time in the same way.
const nodeRequireBanner = `import { createRequire as __zcodeCreateRequire } from "node:module";
const require = __zcodeCreateRequire(import.meta.url);`;

const createBundleOptions = ({ entryPoint, outfile }) => ({
  banner: {
    js: nodeRequireBanner,
  },
  bundle: true,
  entryPoints: [entryPoint],
  format: "esm",
  legalComments: "none",
  outfile,
  platform: "node",
  target: "node24",
});

/**
* A build entry for scripts direct execution and smoke test reuse to ensure that the test and verification products have the same set of esbuild options as the released products.
 */
export const buildBrowserUsePluginBundles = async ({
  packageRoot = defaultPackageRoot,
  browserClientOutfile = resolve(packageRoot, "scripts", "browser-client.mjs"),
} = {}) => {
// The node_repl host product is built and carried by @zcode/node-repl-host itself; this package only comes from browser-client.
  await mkdir(dirname(browserClientOutfile), { recursive: true });
  await build(
    createBundleOptions({
      entryPoint: resolve(packageRoot, "src", "browser-client.ts"),
      outfile: browserClientOutfile,
    }),
  );
  await chmod(browserClientOutfile, executableFileMode);
  return { browserClientOutfile };
};

const entryPath = process.argv[1];
if (entryPath && import.meta.url === pathToFileURL(entryPath).href) {
  await buildBrowserUsePluginBundles();
}
