import { parseModule, type ESTree } from "meriyah";

/**
* REPL code instrument pure function set (route B: top-level binding persists across calls).
 *
*Design motivation:
* In order to support top-level await, NodeReplSession wraps user code into `(async () => {...})()`, resulting in top-level
* `const/let/var/function/class` is captured by IIFE local scope and does not fall into persistent vm context. Use meriyah here
* Parse out the top-level declaration, inject `globalThis.<name> = <name>;` after each declaration statement, and copy the binding to the persistent context,
* Enable the bare name in the next `js` call to be resolved to globalThis via the scope chain.
 *
* This module is A-ready: if you can add `--experimental-vm-modules` in the future to upgrade to SourceTextModule (Route A),
* Just replace the executor and write a new harvest version of the instrument, `parseReplCode`/`collectTopLevelBindingNames` can be reused directly
* (parser replacement implementation only changes parseReplCode internally).
 */

/** parseReplCode result: AST is carried if successful, parseError is carried if failed (no throw, the caller will fall back). */
type ParseReplCodeResult = { ast: ESTree.Program } | { parseError: Error };

/**
* Parse REPL code into ESTree AST. Encapsulate meriyah.parseModule (parser implementation only moves here = A-ready seam).
* Use module mode + next to get the latest syntax; ranges allow each node to have start/end (required for instrument slicing).
* Return { parseError } if parsing fails, never throw (the caller will fall back to the original execution accordingly).
 */
export function parseReplCode(code: string): ParseReplCodeResult {
  try {
    const ast = parseModule(code, { next: true, ranges: true });
    return { ast };
  } catch (error) {
    return { parseError: error instanceof Error ? error : new Error(String(error)) };
  }
}

/**
 * Recursively collect all identifier names declared in a binding pattern.
 * Coverage: Identifier / ObjectPattern (including shorthand and RestElement) / ArrayPattern (including gaps and RestElement)
 * / AssignmentPattern (default) / RestElement.
 */
function collectPatternNames(node: ESTree.Node | null | undefined, out: string[]): void {
  if (!node) {
// The emptiness (elision) in ArrayPattern is null and skipped.
    return;
  }
  switch (node.type) {
    case "Identifier":
      out.push(node.name);
      return;
    case "ObjectPattern":
      for (const prop of node.properties) {
// meriyah actually produces RestElement/Property in the pattern context, but the type annotation is wider
// (ObjectLiteralElementLike contains SpreadElement), so it is narrowed one by one according to type.
        const p = prop as ESTree.Node;
        if (p.type === "RestElement") {
          collectPatternNames(p.argument, out);
        } else if (p.type === "Property") {
// The destructuring target is value (xx of `{x:xx}`; value of shorthand `{x}` is also x).
          collectPatternNames(p.value as ESTree.Node, out);
        } else if (p.type === "SpreadElement") {
// Defensive: When a pattern is rarely marked as SpreadElement, its arguments are also collected.
          collectPatternNames(p.argument as ESTree.Node, out);
        }
      }
      return;
    case "ArrayPattern":
      for (const el of node.elements) {
        collectPatternNames(el, out);
      }
      return;
    case "AssignmentPattern":
// `const {x = 1} = o` / `const [a = 1] = arr`: Binding name is on left.
      collectPatternNames(node.left, out);
      return;
    case "RestElement":
      collectPatternNames(node.argument, out);
      return;
    default:
// Non-declared targets such as MemberExpression (destructuring and assigning values ​​to existing properties) do not generate new bindings and are ignored.
      return;
  }
}

/** Collects all binding names declared by a single top-level declaration statement (for instrument statement-by-statement injection). */
function collectStatementBindingNames(node: ESTree.Node): string[] {
  const names: string[] = [];
  if (node.type === "VariableDeclaration") {
    for (const decl of node.declarations) {
      collectPatternNames(decl.id, names);
    }
  } else if (node.type === "FunctionDeclaration" || node.type === "ClassDeclaration") {
    if (node.id) {
      names.push(node.id.name);
    }
  }
  return names;
}

/**
* Rewrite dynamic import() in cell to injected importModule().
* vm.Script cannot directly execute import expression by default, so the executor uses the host loader and only replaces
* True ImportExpression in AST; strings, comments and import.meta are not affected.
 */
function rewriteDynamicImports(code: string, ast: ESTree.Program): string {
  const starts: number[] = [];
  const seen = new Set<object>();
  const visit = (value: unknown): void => {
    if (!value || typeof value !== "object") return;
    if (seen.has(value)) return;
    seen.add(value);
    if (Array.isArray(value)) {
      for (const item of value) visit(item);
      return;
    }
    const node = value as { type?: unknown; start?: unknown; range?: unknown };
    if (node.type === "ImportExpression") {
      const start =
        typeof node.start === "number"
          ? node.start
          : Array.isArray(node.range) && typeof node.range[0] === "number"
            ? node.range[0]
            : undefined;
      if (start !== undefined) starts.push(start);
    }
    for (const child of Object.values(value as Record<string, unknown>)) visit(child);
  };
  visit(ast);
  let rewritten = code;
  for (const start of starts.sort((left, right) => right - left)) {
    rewritten = `${rewritten.slice(0, start)}importModule${rewritten.slice(start + "import".length)}`;
  }
  return rewritten;
}

/**
* Rewrite dynamic import according to REPL syntax. User code allows top-level return, but module parser does not; when direct parsing fails
* Temporarily wrap it into async function only to obtain reliable AST range, and then peel off the wrapping layer. This will not return regular replacement, nor will
* Mistakenly modify `import(` in a string or comment.
 */
export function rewriteDynamicImportsForRepl(code: string): string {
  const direct = parseReplCode(code);
  if ("ast" in direct) return rewriteDynamicImports(code, direct.ast);

  const prefix = "(async () => {\n";
  const suffix = "\n})";
  const wrapped = `${prefix}${code}${suffix}`;
  const wrappedParsed = parseReplCode(wrapped);
  if (!("ast" in wrappedParsed)) return code;
  const rewritten = rewriteDynamicImports(wrapped, wrappedParsed.ast);
  return rewritten.slice(prefix.length, rewritten.length - suffix.length);
}

/** Get the end offset of the node; end must exist under ranges:true, and the default is to avoid type narrowing problems. */
function nodeEnd(node: ESTree.Node): number {
  const end = node.end ?? node.range?.[1];
  if (end === undefined) {
    throw new Error("instrument requires node end (parseReplCode should have ranges:true)");
  }
  return end;
}

/** Get the starting offset of the node; the same as nodeEnd, used for the completion value slice of the last expression statement. */
function nodeStart(node: ESTree.Node): number {
  const start = node.start ?? node.range?.[0];
  if (start === undefined) {
    throw new Error("instrument requires node start (parseReplCode should have ranges:true)");
  }
  return start;
}

/**
* Copy the binding to the persistent context after each top-level declaration, and convert the last expression statement to return so that the cell
The completion value of * can be returned. Statement-by-statement injection ensures that when subsequent statements throw an error, the statement that was executed before the error is still retained.
 */
export function instrumentForContextPersistence(code: string, ast: ESTree.Program): string {
  let cursor = 0;
  let out = "";
  const lastIndex = ast.body.length - 1;
  ast.body.forEach((stmt, index) => {
//The last top-level expression statement: Convert to return and let async-IIFE return the completion value (REPL echo).
// The declaration/control flow statement does not transfer (its completion value is undefined, and the REPL semantics are consistent).
    if (index === lastIndex && stmt.type === "ExpressionStatement") {
      const stmtStart = nodeStart(stmt);
      const stmtEnd = nodeEnd(stmt);
// meriyah's expression range for `({ value: 1 })` does not contain the outermost bracket.
// Inserting return from expression.start produces `(return ({...});)` illegal syntax; must end with complete
// ExpressionStatement is the boundary, and then only the semicolon at the end of the statement is removed.
      const expressionSource = code.slice(stmtStart, stmtEnd).replace(/;\s*$/, "");
// Keep the original trivia (blank/comment) before the statement, and wrap the complete expression into return (...).
      out += code.slice(cursor, stmtStart);
      out += `return (${expressionSource});`;
      cursor = nodeEnd(stmt);
      return;
    }
    const end = nodeEnd(stmt);
// Keep the original fragment to the end of the statement (including leading whitespace/comments and the statement itself).
    out += code.slice(cursor, end);
    cursor = end;
    const names = collectStatementBindingNames(stmt);
    if (names.length > 0) {
      const assigns = names.map((name) => `globalThis.${name}=${name};`).join("");
      out += `;${assigns}`;
    }
  });
// Keep the trailing original fragment after the last statement.
  out += code.slice(cursor);
  return out;
}
