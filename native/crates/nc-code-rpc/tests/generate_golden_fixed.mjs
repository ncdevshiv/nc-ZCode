import { execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const testsDir = fileURLToPath(new URL(".", import.meta.url));
const workDir = join(testsDir, ".golden-work");
mkdirSync(workDir, { recursive: true });
EOF
