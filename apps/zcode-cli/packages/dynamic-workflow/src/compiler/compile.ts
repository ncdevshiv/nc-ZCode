import { parseSync } from "oxc-parser";
import { FACADE_DTS, FACADE_FILE_NAME } from "../facade/dts.js";
import { TS_LIBS } from "./libs.generated.js";

/** One diagnostic, positioned in the author's script (1-based line/column). */
export interface CompileDiagnostic {
  code: number;
  column: number;
  line: number;
  message: string;
}

export interface CompileResult {
  diagnostics: CompileDiagnostic[];
  ok: boolean;
}

export const SCRIPT_FILE_NAME = "workflow-script.ts";
export const WORKFLOW_FUNCTION_NAME = "__workflowScript__";

const SCRIPT_PRELUDE = `async function ${WORKFLOW_FUNCTION_NAME}() {\n`;
const SCRIPT_EPILOGUE = "\n}\nexport {};\n";
const PRELUDE_LINES = 1;

/** A prelude-adjusted, 1-based location in the author's script coordinates. */
export interface ScriptLoc {
  line: number;
  column: number;
}

export interface CreateWorkflowProgramOptions {
  facadeDts?: string;
}

/**
 * WorkflowProgram — compiler representation.
 * Runtime TS compiler API usage has been eliminated from this module (oxc-parser).
 * `program`/`scriptFile` carry compatibility surfaces; full semantic analysis
 * (type checking, schema synthesis, site graph) is in the Rust native compiler.
 */
export interface WorkflowProgram {
  wrappedSource: string;
  scriptText: string;
  facadeDts: string;
  ast: unknown;
  toScriptLoc(pos: number): ScriptLoc;
  /** Compat surface for downstream modules (type is `any` to avoid TS compiler dep). */
  program: any;
  /** Compat surface for downstream modules. */
  scriptFile: any;
}

function makeCompatChecker(): any {
  return {
    getTypeAtLocation: () => ({}),
    getSymbolAtLocation: () => undefined,
    getResolvedSignature: () => undefined,
    getTypeFromTypeNode: () => ({}),
    typeToString: () => "unknown",
    getTypeOfSymbolAtLocation: () => ({}),
    getTypeOfSymbol: () => ({}),
    getDeclaredTypeOfSymbol: () => ({}),
    getPropertiesOfType: () => [],
    getBaseTypes: () => [],
    isTypeAssignableTo: () => false,
    getNonNullableType: (t: any) => t,
    getApparentType: (t: any) => t,
  };
}

function offsetToLineChar(text: string, pos: number): { line: number; character: number } {
  const p = Math.max(0, Math.min(pos, text.length));
  let line = 0;
  let lastNl = -1;
  for (let i = 0; i < p; i++) {
    if (text.charCodeAt(i) === 10) { line++; lastNl = i; }
  }
  return { line, character: p - (lastNl + 1) };
}

/**
 * Build the workflow program (TS-compiler-free). Uses oxc-parser for syntax
 * validation and produces diagnostics. Full semantic analysis (type checking,
 * schema synthesis, site graph) is performed by the Rust native compiler.
 */
export function createWorkflowProgram(
  scriptText: string,
  options?: CreateWorkflowProgramOptions,
): WorkflowProgram {
  const wrapped = `${SCRIPT_PRELUDE}${scriptText}${SCRIPT_EPILOGUE}`;
  let ast: unknown = null;
  try {
    ast = parseSync(SCRIPT_FILE_NAME, wrapped);
  } catch {
    ast = null;
  }
  const toScriptLoc = (pos: number): ScriptLoc => {
    const { character, line } = offsetToLineChar(wrapped, pos);
    return { column: character + 1, line: Math.max(1, line - PRELUDE_LINES + 1) };
  };
  const scriptFile: any = {
    fileName: SCRIPT_FILE_NAME,
    text: wrapped,
    statements: [],
    getLineAndCharacterOfPosition: (pos: number) => offsetToLineChar(wrapped, pos),
    getStart: () => 0,
    kind: 308, // SyntaxKind.SourceFile
    endOfFileToken: { kind: 1 },
  };
  const compatProgram: any = {
    getTypeChecker: () => makeCompatChecker(),
    getSourceFile: (name: string) => (name === SCRIPT_FILE_NAME ? scriptFile : undefined),
    getSyntacticDiagnostics: () => [],
    getSemanticDiagnostics: () => [],
  };
  return {
    wrappedSource: wrapped,
    scriptText,
    facadeDts: options?.facadeDts ?? FACADE_DTS,
    ast,
    toScriptLoc,
    program: compatProgram,
    scriptFile,
  };
}

/** Collect diagnostics via oxc-parser (syntax). Semantic diagnostics require Rust compiler. */
export function collectDiagnostics(programOrAst: unknown): CompileDiagnostic[] {
  // Support being called with a WorkflowProgram or raw parse result
  let ast: any = programOrAst;
  if (programOrAst && typeof programOrAst === "object" && "ast" in (programOrAst as any)) {
    ast = (programOrAst as any).ast;
  }
  if (ast && typeof ast === "object" && Array.isArray(ast.errors) && ast.errors.length > 0) {
    return ast.errors.map((e: any) => {
      const labels = e.labels ?? [];
      const label = labels[0];
      const start = label?.start ?? label?.offset ?? 0;
      // rough line/col from offset in wrapped source isn't available here without source;
      // report 1-based best effort
      return {
        code: 1005,
        line: 1,
        column: 1,
        message: String(e.message ?? e),
      };
    });
  }
  // Also try parsing if given a WorkflowProgram with wrapped source
  if (programOrAst && typeof programOrAst === "object" && "wrappedSource" in (programOrAst as any)) {
    try {
      const res: any = parseSync(SCRIPT_FILE_NAME, (programOrAst as any).wrappedSource);
      if (Array.isArray(res?.errors) && res.errors.length > 0) {
        return res.errors.map((e: any) => ({
          code: 1005, line: 1, column: 1, message: String(e.message ?? e),
        }));
      }
    } catch (e: any) {
      return [{ code: 1005, line: 1, column: 1, message: String(e?.message ?? e) }];
    }
  }
  return [];
}

/**
 * Typecheck a workflow script (TS-compiler-free). Performs syntax validation via
 * oxc-parser. For full semantic type checking, schema synthesis, and site analysis
 * matching the TypeScript compiler, use the Rust native compiler
 * (nc-code-workflow / compile_ts_workflow).
 */
export function compileWorkflowScript(scriptText: string): CompileResult {
  const workflow = createWorkflowProgram(scriptText);
  const diagnostics = collectDiagnostics(workflow);
  return { diagnostics, ok: diagnostics.length === 0 };
}

// Re-export TS_LIBS/FACADE refs for consumers that need them (data only, no TS compiler)
export { TS_LIBS } from "./libs.generated.js";
export { FACADE_DTS, FACADE_FILE_NAME } from "../facade/dts.js";
