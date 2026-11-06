import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";

const workspaceRoot = fileURLToPath(new URL("../", import.meta.url));
//The current Vite dev server only listens to localhost and does not accept 127.0.0.1.
// Previously, 127.0.0.1 has been polled here, which will cause the preLaunchTask to never be ready, and subsequent Electron startup tasks will not be executed at all.
const viteUrl = "http://localhost:5174";
let child;

async function isViteReady() {
  try {
    const response = await fetch(viteUrl);
    return response.ok;
  } catch {
    return false;
  }
}

async function waitForServer() {
  while (true) {
    if (await isViteReady()) {
      break;
    }

    if (child?.exitCode != null) {
      process.exit(child.exitCode);
    }

    await sleep(300);
  }
}

child = spawn(
  process.platform === "win32" ? "bun.exe" : "bun",
  ["--cwd=packages/desktop", "x", "vite", "dev"],
  {
    cwd: workspaceRoot,
    stdio: "inherit",
  },
);

void waitForServer();

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    if (child != null && !child.killed) {
      child.kill(signal);
      return;
    }

    process.exit(0);
  });
}
if (child != null) {
  child.on("exit", (code, signal) => {
    if (signal != null) {
      process.kill(process.pid, signal);
      return;
    }
    process.exit(code ?? 0);
  });
}
