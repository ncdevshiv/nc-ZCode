import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { resolve } from "node:path";

const workspaceRoot = resolve(import.meta.dirname, "..");
const desktopRoot = resolve(workspaceRoot, "packages/desktop");
const enableHostInspect = process.argv.includes("--host-inspect");

function resolveElectronExecutable() {
  if (process.platform === "darwin") {
    return resolve(
      workspaceRoot,
      "node_modules/electron/dist/Electron.app/Contents/MacOS/Electron",
    );
  }
  if (process.platform === "win32") {
    return resolve(workspaceRoot, "node_modules/electron/dist/electron.exe");
  }
  return resolve(workspaceRoot, "node_modules/electron/dist/electron");
}

const electronExecutable = resolveElectronExecutable();

if (!existsSync(electronExecutable)) {
  console.error(`[zcode-debug] Electron executable not found: ${electronExecutable}`);
  process.exit(1);
}

const child = spawn(electronExecutable, ["--inspect-brk=9231", "."], {
  cwd: desktopRoot,
  stdio: "inherit",
  env: {
    ...process.env,
// When debugging is started, the renderer must also go to localhost.
// Otherwise, even if Vite is ready, Electron will still open a blank page because access to 127.0.0.1 is denied.
    ELECTRON_RENDERER_URL: "http://localhost:5174",
    ...(enableHostInspect ? { ZCODE_DEBUG: "9230" } : {}),
  },
});

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    if (!child.killed) {
      child.kill(signal);
    }
  });
}

child.on("exit", (code, signal) => {
  if (signal != null) {
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 0);
});
