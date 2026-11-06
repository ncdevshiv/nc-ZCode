// Generate shared-contract fixtures for the Rust backend from the REAL Zod schemas.
//
// WHY: `AppSettings` defaults and validation are product rules owned by
// `packages/shared/src/validationAppSettings.ts`. The Rust port must not retype them
// from memory; this script bundles the real schema, validates, and emits:
//   - native/fixtures/app-settings-defaults.json  (`appSettingsSchema.parse({})`)
//   - native/fixtures/app-settings.schema.json    (JSON Schema, input mode)
//   - native/fixtures/app-settings-patch.schema.json
//
//   node native/tools/generate-shared-contracts.mjs

import { createRequire } from "node:module";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";

const repoRoot = fileURLToPath(new URL("../../", import.meta.url));
const require = createRequire(import.meta.url);
const esbuild = require(join(repoRoot, "node_modules", "esbuild", "lib", "main.js"));

const workDir = join(tmpdir(), `zcode-shared-contracts-${process.pid}`);
mkdirSync(workDir, { recursive: true });
const bundlePath = join(workDir, "shared.cjs");
esbuild.buildSync({
  stdin: {
    contents: `
      export { appSettingsSchema, appSettingsPatchSchema } from "./validationAppSettings.js";
      export { z } from "zod";
    `,
    resolveDir: join(repoRoot, "packages", "shared", "src"),
    sourcefile: "contracts-entry.ts",
    loader: "ts",
  },
  bundle: true,
  format: "cjs",
  platform: "node",
  outfile: bundlePath,
  logLevel: "silent",
});

const { appSettingsSchema, appSettingsPatchSchema, z } = require(bundlePath);

const defaults = appSettingsSchema.parse({});
const appSettingsJsonSchema = z.toJSONSchema(appSettingsSchema, {
  io: "input",
  unrepresentable: "any",
});
const appSettingsPatchJsonSchema = z.toJSONSchema(appSettingsPatchSchema, {
  io: "input",
  unrepresentable: "any",
});

const fixturesDir = join(repoRoot, "native", "fixtures");
writeFileSync(
  join(fixturesDir, "app-settings-defaults.json"),
  `${JSON.stringify(defaults, null, 2)}\n`,
);
writeFileSync(
  join(fixturesDir, "app-settings.schema.json"),
  `${JSON.stringify(appSettingsJsonSchema, null, 2)}\n`,
);
writeFileSync(
  join(fixturesDir, "app-settings-patch.schema.json"),
  `${JSON.stringify(appSettingsPatchJsonSchema, null, 2)}\n`,
);

process.stderr.write(
  `defaults keys=${Object.keys(defaults).length} schema=${
    Object.keys(appSettingsJsonSchema.properties ?? {}).length
  } patch=${Object.keys(appSettingsPatchJsonSchema.properties ?? {}).length}\n`,
);

// Sanity: the schema must accept its own defaults (catches generator/version drift).
appSettingsSchema.parse(defaults);
