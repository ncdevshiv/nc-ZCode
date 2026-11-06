import type { RuntimeInfo } from "@zcode/shared-types";
export { type RuntimeInfo };
type SeaModule = { isSea?: () => boolean };
const getBuiltinModule = (
  process as unknown as { getBuiltinModule?: (id: string) => SeaModule }
).getBuiltinModule;
// `node:sea` only exists in Node's single-executable runtime; Bun and plain Node report false.
const sea = getBuiltinModule?.("node:sea")?.isSea?.() === true;
export const getRuntimeInfo = (): RuntimeInfo => ({
  arch: process.arch,
  cwd: process.cwd(),
  execPath: process.execPath,
  node: process.version,
  platform: process.platform,
  sea,
  versions: process.versions,
});
