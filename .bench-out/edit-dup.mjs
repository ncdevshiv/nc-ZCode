import { writeFileSync, readFileSync, mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
const base = process.cwd();
const imp = (p) => import(pathToFileURL(join(base, p)).href);
const { editToolEntry } = await imp("apps/zcode-cli/packages/core/dist/tool/handlers/edit.js");
const { readToolEntry } = await imp("apps/zcode-cli/packages/core/dist/tool/handlers/read.js");
const { createNodeFileSystemAdapter } = await imp("apps/zcode-cli/packages/adapters/dist/fs/index.js");
const ws = join(tmpdir(), "zcode-edit-dup-" + process.pid);
mkdirSync(ws, { recursive: true });
const file = join(ws, "dup.txt");
writeFileSync(file, "value-v1\nvalue-v1\nvalue-v1\n");
const ctx = () => shared; const shared = ({ workspaceRoot: ws, workingDirectory: ws, abortSignal: new AbortController().signal,
  toolCallId: "c", traceId: "t", sessionId: "s", turnId: "u", runtimeScope: "main",
  fileSystemPort: createNodeFileSystemAdapter(), readFileState: new Map() });
const one = ctx();
await readToolEntry.handler({ file_path: file }, one);
try {
  const out = await editToolEntry.handler({ file_path: file, old_string: "value-v1", new_string: "value-v2" }, one);
  console.log("TS edit result: OK", JSON.stringify(out).slice(0, 200));
  console.log("file now:", JSON.stringify(readFileSync(file, "utf8")));
} catch (error) {
  console.log("TS edit rejected:", error.message);
}
rmSync(ws, { recursive: true, force: true });
