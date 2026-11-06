/**
 * TS-compiler-API-free shim. Replaces `import ts from "typescript"` package across the
 * workflow analysis pipeline. Runtime is stubbed; types are permissive for compatibility.
 * Authoritative analysis is implemented in Rust (nc-code-workflow).
 */
const SyntaxKind: any = new Proxy({}, { get: (_, k) => (typeof k === "string" ? k : 0) });
const SymbolFlags: any = new Proxy({}, { get: () => 0 });
const TypeFlags: any = new Proxy({}, { get: () => 0 });
const NodeFlags: any = new Proxy({}, { get: () => 0 });
const ModifierFlags: any = new Proxy({}, { get: () => 0 });

const noopBool = (..._a: any[]) => false;
const noopAny = (..._a: any[]) => undefined;

const tsValue: any = {
  SyntaxKind, SymbolFlags, TypeFlags, NodeFlags, ModifierFlags,
  isCallExpression: (n: any) => n?.type === "CallExpression",
  isPropertyAccessExpression: (n: any) => n?.type === "MemberExpression" && !n?.computed,
  isElementAccessExpression: (n: any) => n?.type === "MemberExpression" && !!n?.computed,
  isIdentifier: (n: any) => n?.type === "Identifier",
  isStringLiteral: (n: any) => n?.type === "Literal" && typeof n?.value === "string",
  isStringLiteralLike: (n: any) => (n?.type === "Literal" && typeof n?.value === "string") || n?.type === "TemplateLiteral",
  isNoSubstitutionTemplateLiteral: (n: any) => n?.type === "TemplateLiteral" && (n?.expressions?.length ?? 0) === 0,
  isTemplateExpression: (n: any) => n?.type === "TemplateLiteral" && (n?.expressions?.length ?? 0) > 0,
  isNumericLiteral: (n: any) => n?.type === "Literal" && typeof n?.value === "number",
  isExpressionStatement: (n: any) => n?.type === "ExpressionStatement",
  isVariableDeclaration: (n: any) => n?.type === "VariableDeclarator",
  isVariableStatement: (n: any) => n?.type === "VariableDeclaration",
  isFunctionDeclaration: (n: any) => n?.type === "FunctionDeclaration",
  isFunctionExpression: (n: any) => n?.type === "FunctionExpression",
  isArrowFunction: (n: any) => n?.type === "ArrowFunctionExpression",
  isMethodDeclaration: (n: any) => n?.type === "MethodDefinition",
  isBlock: (n: any) => n?.type === "BlockStatement",
  isReturnStatement: (n: any) => n?.type === "ReturnStatement",
  isIfStatement: (n: any) => n?.type === "IfStatement",
  isForOfStatement: (n: any) => n?.type === "ForOfStatement",
  isForInStatement: (n: any) => n?.type === "ForInStatement",
  isForStatement: (n: any) => n?.type === "ForStatement",
  isWhileStatement: (n: any) => n?.type === "WhileStatement",
  isDoStatement: (n: any) => n?.type === "DoWhileStatement",
  isTryStatement: (n: any) => n?.type === "TryStatement",
  isAwaitExpression: (n: any) => n?.type === "AwaitExpression",
  isBinaryExpression: (n: any) => n?.type === "BinaryExpression" || n?.type === "AssignmentExpression" || n?.type === "LogicalExpression",
  isConditionalExpression: (n: any) => n?.type === "ConditionalExpression",
  isParenthesizedExpression: (n: any) => n?.type === "ParenthesizedExpression",
  isAsExpression: (n: any) => n?.type === "TSAsExpression",
  isTypeAssertionExpression: (n: any) => n?.type === "TSTypeAssertion",
  isNonNullExpression: (n: any) => n?.type === "TSNonNullExpression",
  isSatisfiesExpression: (n: any) => n?.type === "TSSatisfiesExpression",
  isSpreadElement: (n: any) => n?.type === "SpreadElement",
  isArrayLiteralExpression: (n: any) => n?.type === "ArrayExpression",
  isObjectLiteralExpression: (n: any) => n?.type === "ObjectExpression",
  isPropertyAssignment: (n: any) => n?.type === "Property" && !n?.method && !n?.shorthand,
  isShorthandPropertyAssignment: (n: any) => n?.type === "Property" && !!n?.shorthand,
  isSpreadAssignment: (n: any) => n?.type === "SpreadElement",
  isNewExpression: (n: any) => n?.type === "NewExpression",
  isPrefixUnaryExpression: (n: any) => n?.type === "UnaryExpression",
  isPostfixUnaryExpression: (n: any) => n?.type === "UpdateExpression" && !n?.prefix,
  isTypeOfExpression: noopBool,
  isVoidExpression: (n: any) => n?.type === "UnaryExpression" && n?.operator === "void",
  isDeleteExpression: (n: any) => n?.type === "UnaryExpression" && n?.operator === "delete",
  isBreakStatement: (n: any) => n?.type === "BreakStatement",
  isContinueStatement: (n: any) => n?.type === "ContinueStatement",
  isThrowStatement: (n: any) => n?.type === "ThrowStatement",
  isSwitchStatement: (n: any) => n?.type === "SwitchStatement",
  isLabeledStatement: (n: any) => n?.type === "LabeledStatement",
  isClassDeclaration: (n: any) => n?.type === "ClassDeclaration",
  isClassExpression: (n: any) => n?.type === "ClassExpression",
  isInterfaceDeclaration: (n: any) => n?.type === "TSInterfaceDeclaration",
  isTypeAliasDeclaration: (n: any) => n?.type === "TSTypeAliasDeclaration",
  isEnumDeclaration: (n: any) => n?.type === "TSEnumDeclaration",
  isModuleDeclaration: (n: any) => n?.type === "TSModuleDeclaration",
  isImportDeclaration: (n: any) => n?.type === "ImportDeclaration",
  isExportDeclaration: (n: any) => n?.type === "ExportNamedDeclaration" || n?.type === "ExportDefaultDeclaration",
  isExportAssignment: (n: any) => n?.type === "TSExportAssignment",
  isSourceFile: (n: any) => n?.type === "Program",
  isToken: noopBool,
  isPrivateIdentifier: (n: any) => n?.type === "PrivateIdentifier",
  isComputedPropertyName: (n: any) => !!n?.computed,
  isGetAccessorDeclaration: noopBool, isSetAccessorDeclaration: noopBool, isConstructorDeclaration: noopBool,
  forEachChild: (node: any, cb: (n: any) => any): any => {
    if (!node || typeof node !== "object") return undefined;
    const visit = (v: any): any => {
      if (!v) return undefined;
      if (Array.isArray(v)) { for (const x of v) { if (x && typeof x.type === "string") { const r = cb(x); if (r) return r; } } return undefined; }
      if (typeof v.type === "string") return cb(v);
      return undefined;
    };
    for (const k of Object.keys(node)) {
      if (k === "type" || k === "start" || k === "end" || k === "loc" || k === "range" || k === "span" || k === "parent") continue;
      const r = visit(node[k]); if (r) return r;
    }
    return undefined;
  },
  getOriginalNode: (n: any) => n,
  idText: (n: any) => n?.name ?? n?.escapedText ?? "",
  getTextOfNode: noopAny,
  displayPartsToString: (parts: any) => Array.isArray(parts) ? parts.map((p:any)=>p?.text??"").join("") : "",
  createSourceFile: noopAny, createProgram: noopAny, parseJsonText: noopAny,
  ScriptTarget: new Proxy({}, { get: () => 99 }), ScriptKind: new Proxy({}, { get: () => 0 }),
  ModuleKind: new Proxy({}, { get: () => 99 }), ModuleResolutionKind: new Proxy({}, { get: () => 0 }),
  JsxEmit: new Proxy({}, { get: () => 0 }),
  getJSDocTags: () => [], getJSDocCommentsAndTags: () => [],
  getLeadingCommentRanges: () => [],
  flattenDiagnosticMessageText: (m: any) => typeof m === "string" ? m : String(m?.messageText ?? m ?? ""),
  getLineAndCharacterOfPosition: (_sf: any, pos: number) => ({ line: 0, character: pos }),
  getPreEmitDiagnostics: () => [],
  sys: { useCaseSensitiveFileNames: true, fileExists: () => false, readFile: () => undefined, directoryExists: () => false, getCurrentDirectory: () => ".", readDirectory: () => [] },
  version: "0.0.0-shim",
};

// Provide namespace types for compatibility with `ts.Node` style usage
export namespace ts {
  export type Node = any;
  export type SourceFile = any;
  export type TypeChecker = any;
  export type Expression = any;
  export type Statement = any;
  export type Symbol = any;
  export type Type = any;
  export type Program = any;
  export type Diagnostic = any;
  export type CallExpression = any;
  export type Identifier = any;
  export type PropertyAccessExpression = any;
  export type ElementAccessExpression = any;
  export type StringLiteral = any;
  export type NumericLiteral = any;
  export type Block = any;
  export type FunctionDeclaration = any;
  export type ArrowFunction = any;
  export type VariableDeclaration = any;
  export type IfStatement = any;
  export type ForOfStatement = any;
  export type ReturnStatement = any;
  export type AwaitExpression = any;
  export type BinaryExpression = any;
  export type ObjectLiteralExpression = any;
  export type ArrayLiteralExpression = any;
  export type ParameterDeclaration = any;
  export type TypeNode = any;
  export type JSDoc = any;
  export type SyntaxKind = number;
  export type CompilerOptions = any;
  export type ScriptTarget = number;
  export type ScriptKind = number;
  export type ModuleKind = number;
  export type ModuleResolutionKind = number;
  export type JsxEmit = number;
  export type TransformationContext = any;
  export type TransformerFactory<T extends Node> = (context: TransformationContext) => (rootNode: T) => T;
  export type Visitor = (node: Node) => Node;
  export type Printer = any;
  export type EmitResult = any;
  export type SymbolFlags = number;
  export type TypeFlags = number;
  export type NodeFlags = number;
  export type ModifierFlags = number;
}

const ts = tsValue;
export default ts;
export { ts };
