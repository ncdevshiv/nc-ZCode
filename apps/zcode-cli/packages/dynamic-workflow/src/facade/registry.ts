// @ts-nocheck
/**
 * Facade registry - TS-compiler-API-free.
 * Pure data: (container, member) -> op/kind for world-read and artifact facade calls.
 * Symbol-resolution helpers accept duck-typed objects (for compat with legacy TS Program
 * pipelines) but do NOT import the `typescript` package.
 */
import { FACADE_FILE_NAME } from "./dts.js";

export interface WorldReadRow {
  readonly container: string;
  readonly member: string;
  readonly op: string;
}

export const WORLD_READ_REGISTRY = [
  { container: "files", member: "glob", op: "glob" },
  { container: "files", member: "read", op: "read" },
  { container: "files", member: "grep", op: "grep" },
  { container: "git", member: "changedFiles", op: "git-changed-files" },
  { container: "git", member: "diff", op: "git-diff" },
  { container: "git", member: "status", op: "git-status" },
  { container: "git", member: "log", op: "git-log" },
  { container: "world", member: "run", op: "run" },
] as const satisfies readonly WorldReadRow[];

export type WorldReadOp = (typeof WORLD_READ_REGISTRY)[number]["op"];

export interface ArtifactRow {
  readonly container: "artifact";
  readonly member: string;
  readonly op: string;
  readonly family: "content" | "preset";
}

export const ARTIFACT_REGISTRY = [
  { container: "artifact", member: "file", op: "file", family: "content" },
  { container: "artifact", member: "markdown", op: "markdown", family: "content" },
  { container: "artifact", member: "chart", op: "chart", family: "preset" },
  { container: "artifact", member: "table", op: "table", family: "preset" },
  { container: "artifact", member: "metrics", op: "metrics", family: "preset" },
  { container: "artifact", member: "board", op: "board", family: "preset" },
] as const satisfies readonly ArtifactRow[];

export type ArtifactOp = (typeof ARTIFACT_REGISTRY)[number]["op"];
export type ArtifactContentOp = Extract<(typeof ARTIFACT_REGISTRY)[number], { family: "content" }>["op"];
export type ArtifactPresetOp = Extract<(typeof ARTIFACT_REGISTRY)[number], { family: "preset" }>["op"];

function artifactRow(container: string | undefined, member: string): (typeof ARTIFACT_REGISTRY)[number] | undefined {
  if (container === undefined) return undefined;
  return ARTIFACT_REGISTRY.find((row) => row.container === container && row.member === member);
}

export function artifactFamilyOf(op: ArtifactOp): "content" | "preset" {
  const row = ARTIFACT_REGISTRY.find((candidate) => candidate.op === op);
  if (row === undefined) throw new Error(`unknown artifact op: ${op}`);
  return row.family;
}

export function isArtifactPresetOp(op: string): op is ArtifactPresetOp {
  return ARTIFACT_REGISTRY.some((row) => row.op === op && row.family === "preset");
}

const ASK_MEMBER = { container: "Agent", member: "ask" } as const;
const SITE_PRODUCING_FUNCTIONS = ["agent", "report"] as const;
type SiteProducingFunction = (typeof SITE_PRODUCING_FUNCTIONS)[number];
const MARKER_FUNCTIONS = ["phase"] as const;
type MarkerFunction = (typeof MARKER_FUNCTIONS)[number];

export const SITE_MEMBER_NAMES: ReadonlySet<string> = new Set<string>([
  ASK_MEMBER.member,
  ...WORLD_READ_REGISTRY.map((row) => row.member),
  ...ARTIFACT_REGISTRY.map((row) => row.member),
]);

function worldReadOp(container: string | undefined, member: string): WorldReadOp | undefined {
  if (container === undefined) return undefined;
  return WORLD_READ_REGISTRY.find((row) => row.container === container && row.member === member)?.op;
}

export function isSiteProducing(container: string | undefined, member: string): boolean {
  if (container === undefined) return SITE_PRODUCING_FUNCTIONS.some((name) => name === member);
  if (container === ASK_MEMBER.container && member === ASK_MEMBER.member) return true;
  if (artifactRow(container, member) !== undefined) return true;
  return worldReadOp(container, member) !== undefined;
}

/** Pure lookup by (container, member) - TS-compiler-free. */
export function worldReadOpByName(container: string | undefined, member: string): WorldReadOp | undefined {
  return worldReadOp(container, member);
}
export function artifactRowByName(container: string | undefined, member: string): (typeof ARTIFACT_REGISTRY)[number] | undefined {
  return artifactRow(container, member);
}

/* --- Duck-typed symbol helpers (no `typescript` import). Used by legacy analysis passes
   when a real TS Program was present; with the TS-compiler-free compile.ts they receive
   compat stubs and return undefined, which is safe because site collection is handled
   by the Rust native compiler / oxc-based paths. --- */

function isFacadeDecl(decl: any): boolean {
  try { return decl?.getSourceFile?.()?.fileName === FACADE_FILE_NAME; } catch { return false; }
}

export function facadeContainerOf(declaration: any): string | undefined {
  if (declaration === undefined || declaration === null) return undefined;
  if (!isFacadeDecl(declaration)) return undefined;
  for (let node: any = declaration.parent; node != null; node = node.parent) {
    // VariableDeclaration: ts.isVariableDeclaration equivalent via kind duck-typing avoided; use name presence
    if (node.name && typeof node.name.text === "string" && node.initializer !== undefined) {
      // could be VariableDeclaration
      const k = node.kind;
      // VariableDeclaration=257 in TS, but we avoid importing; check shape
      if (k === 257) return node.name.text;
    }
    if (node.name && typeof node.name.text === "string" && (node.kind === 261 || node.kind === 262)) {
      // InterfaceDeclaration=261, TypeAliasDeclaration=262
      return node.name.text;
    }
    if (node.kind === 308) return undefined; // SourceFile
  }
  return undefined;
}

function facadeMemberOf(symbol: any): { container: string | undefined; member: string } | undefined {
  const decls = symbol?.declarations;
  if (!Array.isArray(decls)) return undefined;
  const declaration = decls.find((d: any) => isFacadeDecl(d));
  if (declaration === undefined || symbol === undefined) return undefined;
  return { container: facadeContainerOf(declaration), member: String(symbol.name ?? "") };
}

export function artifactRowOfSymbol(symbol: any): (typeof ARTIFACT_REGISTRY)[number] | undefined {
  const member = facadeMemberOf(symbol);
  if (member === undefined) return undefined;
  return artifactRow(member.container, member.member);
}

export function worldReadOpOfSymbol(symbol: any): WorldReadOp | undefined {
  const member = facadeMemberOf(symbol);
  if (member === undefined) return undefined;
  return worldReadOp(member.container, member.member);
}

export function siteProducingFunctionOfSymbol(symbol: any): SiteProducingFunction | undefined {
  const member = facadeMemberOf(symbol);
  if (member === undefined || member.container !== undefined) return undefined;
  return SITE_PRODUCING_FUNCTIONS.find((name) => name === member.member);
}

export function markerFunctionOfSymbol(symbol: any): MarkerFunction | undefined {
  const member = facadeMemberOf(symbol);
  if (member === undefined || member.container !== undefined) return undefined;
  return MARKER_FUNCTIONS.find((name) => name === member.member);
}
