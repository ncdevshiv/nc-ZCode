// @ts-nocheck
import ts from "../compiler/ts-shim.js";
import { isFunctionLike } from "./sites.js";
import type { ApplicationVia } from "./state.js";

// causality-order.ts reaches the upper limit of oxlint max-lines (400 lines), making the function parsing class a pure auxiliary
// (script function determination, outer function chain, function naming, call statement parsing, recursive SCC precalculation, binding name expansion) split
// This file; the public side is still exported from causality-order.ts. None of the functions here read the walkthrough status, only the AST and checker.

export type ScriptFunction = ts.SignatureDeclaration & { body: ts.Node };

/** Identifiers a binding name introduces (an identifier, or every leaf of a pattern). */
export function boundIdentifiers(name: ts.BindingName): ts.Identifier[] {
  if (ts.isIdentifier(name)) return [name];
  const out: ts.Identifier[] = [];
  for (const element of name.elements) {
    if (ts.isOmittedExpression(element)) continue;
    out.push(...boundIdentifiers(element.name));
  }
  return out;
}

export function asScriptFunction(
  node: ts.Node,
  scriptFile: ts.SourceFile,
): ScriptFunction | undefined {
  if (!isFunctionLike(node)) return undefined;
  if (node.getSourceFile() !== scriptFile) return undefined;
  const body = (node as ts.SignatureDeclaration & { body?: ts.Node }).body;
  return body === undefined ? undefined : (node as ScriptFunction);
}

/** The function-like ancestors of a node, innermost first. */
export function enclosingFunctions(node: ts.Node): ScriptFunction[] {
  const scriptFile = node.getSourceFile();
  const out: ScriptFunction[] = [];
  for (let current = node.parent; current !== undefined; current = current.parent) {
    const fn = asScriptFunction(current, scriptFile);
    if (fn !== undefined) out.push(fn);
  }
  return out;
}

export function functionName(decl: ts.Node): string | undefined {
  // A constructor is named by its class: `new Judge()` inlines as a call labelled "Judge".
  if (ts.isConstructorDeclaration(decl)) {
    const cls = decl.parent;
    return ts.isClassLike(cls) && cls.name !== undefined ? cls.name.text : undefined;
  }
  const named = decl as { name?: ts.Node };
  if (named.name !== undefined && ts.isIdentifier(named.name)) return named.name.text;
  const parent = decl.parent;
  if (parent !== undefined && ts.isVariableDeclaration(parent) && ts.isIdentifier(parent.name)) {
    return parent.name.text;
  }
  return undefined;
}

/**
 * The script-local function a call resolves to. `getResolvedSignature` handles the
 * hard shapes (methods, overloads, aliased consts) in one hop; anything outside the
 * authored file (the facade, the stdlib) is not inlinable.
 */
export function resolveCallDeclaration(
  call: ts.CallExpression,
  checker: ts.TypeChecker,
  scriptFile: ts.SourceFile,
): ScriptFunction | undefined {
  const declaration = checker.getResolvedSignature(call)?.declaration;
  if (declaration === undefined) return undefined;
  return asScriptFunction(declaration, scriptFile);
}

/**
 * Script-local functions that can reach themselves in the call graph — the SCCs the analysis turns into `loop` regions instead of unrolling. Callees are the union of the
 * checker's resolved declarations and the call oracle's applications (both vias: a
 * function handed to a library that hands it back is a cycle too). Nested function bodies
 * count as their owner's callees, which over-approximates cycles; over-approximating here
 * only costs a `loop` region, while under-approximating would unroll forever.
 */
export function collectRecursiveFunctions(
  scriptFile: ts.SourceFile,
  checker: ts.TypeChecker,
  applications: ReadonlyMap<ts.Node, ReadonlyMap<ts.Node, ApplicationVia>>,
): Set<ts.Node> {
  const functions: ScriptFunction[] = [];
  const collect = (node: ts.Node): void => {
    const fn = asScriptFunction(node, scriptFile);
    if (fn !== undefined) functions.push(fn);
    ts.forEachChild(node, collect);
  };
  collect(scriptFile);

  const callees = new Map<ts.Node, Set<ts.Node>>();
  for (const fn of functions) {
    const set = new Set<ts.Node>();
    const scan = (node: ts.Node): void => {
      if (ts.isCallExpression(node)) {
        const target = resolveCallDeclaration(node, checker, scriptFile);
        if (target !== undefined) set.add(target);
      }
      if (
        ts.isCallExpression(node) ||
        ts.isNewExpression(node) ||
        ts.isTaggedTemplateExpression(node)
      ) {
        for (const applied of applications.get(node)?.keys() ?? []) {
          const target = asScriptFunction(applied, scriptFile);
          if (target !== undefined) set.add(target);
        }
      }
      ts.forEachChild(node, scan);
    };
    scan(fn.body);
    callees.set(fn, set);
  }

  const recursive = new Set<ts.Node>();
  for (const fn of functions) {
    const seen = new Set<ts.Node>();
    const stack = [...(callees.get(fn) ?? [])];
    while (stack.length > 0) {
      const next = stack.pop() as ts.Node;
      if (next === fn) {
        recursive.add(fn);
        break;
      }
      if (seen.has(next)) continue;
      seen.add(next);
      stack.push(...(callees.get(next) ?? []));
    }
  }
  return recursive;
}
