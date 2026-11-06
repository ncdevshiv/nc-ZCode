// @ts-nocheck
import ts from "../compiler/ts-shim.js";
import { FACADE_FILE_NAME } from "../facade/dts.js";
import type { WorldReadOp } from "../facade/registry.js";
import type { ScriptLoc } from "../compiler/compile.js";
import type { NamePattern } from "./types.js";
import type { CallbackSemantics } from "./callbacks.js";
import type { IterationCandidate } from "./sites.js";

// sites.ts reaches the upper limit of oxlint max-lines (400 lines), and adds name/tag auxiliary (ask tag and template
// shapes, actor names and template shapes, world-read labels, literal literals, constructors of iteration candidates, counters) and their dependencies
// Symbol resolution (resolveSymbol / isFacadeDeclared) is split into this file; the public side is still exported from sites.ts (the latter two
//Export it there as it is, the rest will not be made public). Site types stay in sites.ts, here they are only imported by type.

/** Resolve a node's symbol, following one alias hop (imports never occur here). */
export function resolveSymbol(node: ts.Node, checker: ts.TypeChecker): ts.Symbol | undefined {
  const symbol = checker.getSymbolAtLocation(node);
  if (symbol !== undefined && (symbol.flags & ts.SymbolFlags.Alias) !== 0) {
    return checker.getAliasedSymbol(symbol);
  }
  return symbol;
}

/** True iff any of the symbol's declarations lives in the facade `.d.ts`. */
export function isFacadeDeclared(symbol: ts.Symbol | undefined): boolean {
  return (
    symbol?.declarations?.some(
      (declaration) => declaration.getSourceFile().fileName === FACADE_FILE_NAME,
    ) ?? false
  );
}

/**
* The text of an actual parameter **holeless string literal**, the rest (identifiers, templates with holes, arbitrary expressions) are all undefined.
 *
* Three places share one judgment: the name of the phase, the id of the product, and the label of the report. The rules of the three are exactly the same - only at compile time
* Only closed literals count. Those that cannot be seen through will be left to their own diagnosis and positioned according to the original expression. ** Never guess a value here.
* comes out ** (the guessed name silently passes a call that should have been rewritten). `ts.isStringLiteralLike` override
* Backtick string without interpolation.
 */
export function literalText(expr: ts.Expression | undefined): string | undefined {
  return expr !== undefined && ts.isStringLiteralLike(expr) ? expr.text : undefined;
}

export class Counter {
  private value = 0;
  next(): number {
    this.value += 1;
    return this.value;
  }
}

/**
 * ask label: an inline `agent("name", …)` receiver contributes its literal name;
 * a plain identifier receiver contributes its text; otherwise `"ask"`.
 */
export function askLabel(receiver: ts.Expression, checker: ts.TypeChecker): string {
  if (ts.isCallExpression(receiver) && isActorCall(receiver, checker)) {
    const first = receiver.arguments[0];
    if (first !== undefined && ts.isStringLiteralLike(first)) return first.text;
  }
  if (ts.isIdentifier(receiver)) return receiver.text;
  return "ask";
}

/**
* ask label pattern: only for an inline `` agent(`researcher${i}`) `` receiver — the one shape
 * `askLabel` answers `"ask"` for despite the script having said something. A named or
 * identifier receiver already produced a real label, so there is nothing to reconstruct.
 */
export function askLabelPattern(
  receiver: ts.Expression,
  checker: ts.TypeChecker,
): NamePattern | undefined {
  if (!ts.isCallExpression(receiver) || !isActorCall(receiver, checker)) return undefined;
  return templateAffixes(receiver.arguments[0]);
}

function isActorCall(call: ts.CallExpression, checker: ts.TypeChecker): boolean {
  if (ts.isPropertyAccessExpression(call.expression)) return false;
  const symbol = resolveSymbol(call.expression, checker);
  return isFacadeDeclared(symbol) && symbol?.name === "agent";
}

/**
 * actor name: a string-literal first argument, else the binding name when the call
 * directly initializes a variable declaration, else undefined.
 */
export function actorName(call: ts.CallExpression): string | undefined {
  const first = call.arguments[0];
  if (first !== undefined && ts.isStringLiteralLike(first)) return first.text;
  const parent = call.parent;
  if (
    ts.isVariableDeclaration(parent) &&
    parent.initializer === call &&
    ts.isIdentifier(parent.name)
  ) {
    return parent.name.text;
  }
  return undefined;
}

/**
 * actor name pattern: the static shape of a first argument that is a template literal
 * with holes. Only consulted when `actorName` came back undefined — a literal name and a
 * binding name are both real names, and a pattern must never displace one.
 */
export function actorNamePattern(call: ts.CallExpression): NamePattern | undefined {
  return templateAffixes(call.arguments[0]);
}

/**
* Literals at both ends of the template string: before the first hole (`head`) and after the last hole (`tail`).
 *
* `ts.isStringLiteralLike` already covers backtick strings without interpolation (that's literal, go the `name` way),
* So only `ts.TemplateExpression` with real holes is processed here.
 *
* **Intermediate literals intentionally discarded**: `` `a${x}b${y}c` `` gives `a` and `c`, not `a…b…c`. name is one line
The label on * is not a rendering of the expression that produced it.
 */
function templateAffixes(arg: ts.Expression | undefined): NamePattern | undefined {
  if (arg === undefined || !ts.isTemplateExpression(arg)) return undefined;
  const head = meaningfulAffix(arg.head.text);
  const tail = meaningfulAffix(arg.templateSpans.at(-1)?.literal.text);
  if (head === undefined && tail === undefined) return undefined;
  return {
    ...(head === undefined ? {} : { head }),
    ...(tail === undefined ? {} : { tail }),
  };
}

/**
* An affix value is not worth displaying: there must be at least one letter or number after trim.
 *
* Why this door is needed: `` agent(`${x}-`) ``'s tail is `-`, which is rendered as `...-` - that's better than "unnamed agent"
* Worse, readers can’t get the name, but also have a bunch of punctuation marks. `\p{L}` overrides CJK, so "researcher" passes as usual.
 */
function meaningfulAffix(text: string | undefined): string | undefined {
  const trimmed = text?.trim();
  if (trimmed === undefined || trimmed === "") return undefined;
  return /[\p{L}\p{N}]/u.test(trimmed) ? trimmed : undefined;
}

/** world-read label: the op name, plus a string-literal first argument when present. */
export function worldReadLabel(op: WorldReadOp, arg: ts.Expression | undefined): string {
  if (arg !== undefined && ts.isStringLiteralLike(arg)) return `${op} ${arg.text}`;
  return op;
}

export function eachCandidate(
  call: ts.CallExpression,
  semantics: CallbackSemantics,
  order: number,
  loc: ScriptLoc,
): IterationCandidate | undefined {
  const index = semantics.callbacks[0];
  const argument = index === undefined ? undefined : call.arguments[index];
  const iterated = semantics.iterated;
  if (argument === undefined || iterated === undefined) return undefined;
  const literal = ts.isArrowFunction(argument) || ts.isFunctionExpression(argument);
  return {
    body: literal ? argument.body : argument,
    ...(literal ? { callback: argument } : { callbackExpr: argument }),
    call,
    element: literal ? argument.parameters[semantics.elementParams?.[0] ?? 0]?.name : undefined,
    form: "array-method",
    iterated,
    loc,
    method: semantics.label,
    order,
    semantics,
  };
}

export function forOfCandidate(
  statement: ts.ForOfStatement,
  order: number,
  loc: ScriptLoc,
): IterationCandidate {
  let element: ts.BindingName | undefined;
  if (ts.isVariableDeclarationList(statement.initializer)) {
    element = statement.initializer.declarations[0]?.name;
  }
  return {
    body: statement.statement,
    element,
    form: "for-of",
    iterated: statement.expression,
    loc,
    order,
  };
}
