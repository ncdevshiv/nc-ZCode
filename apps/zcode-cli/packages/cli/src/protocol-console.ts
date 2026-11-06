import { Console } from "node:console";

/**
* Install console output boundary for CLI (stdio protocol/TUI) that exclusive stdout.
 *
* The stdout of app-server/agent-server can only carry ZCode Protocol NDJSON frames, but the third party
* The SDK may write plain text via console.*. If you don't separate the output before loading these dependencies,
* Any line of logs will be parsed as JSON by the Host, or directly overwrite the screen at the current cursor position of the TUI.
 */
export function installStderrConsoleBoundary(stderr: NodeJS.WritableStream): () => void {
  const originalConsole = globalThis.console;
  globalThis.console = new Console({ stdout: stderr, stderr });

  let restored = false;
  return () => {
    if (restored) return;
    restored = true;
    globalThis.console = originalConsole;
  };
}
