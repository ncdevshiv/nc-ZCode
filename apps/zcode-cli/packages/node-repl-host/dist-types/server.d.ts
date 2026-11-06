import { Server } from "@modelcontextprotocol/server";
import { type NodeReplRequestMeta, type NodeReplRunResult } from "@zcode/core/repl";
import { type ComputerUseRuntime } from "@zcode/zcode-cua";
import { type NodeReplCuaBrokerConnection } from "./cua-bridge.js";
import { installNodeReplProcessGuards, installNodeReplShutdownTriggers } from "./process-lifecycle.js";
export declare const NODE_REPL_MCP_PROCESS_TITLE = "zcode-node-repl-mcp";
export interface NodeReplExecuteInput {
    code: string;
    requestMeta: NodeReplRequestMeta;
    signal: AbortSignal;
    syncTimeoutMs: number;
    cuaBroker?: NodeReplCuaBrokerConnection;
}
export type NodeReplExecutor = (input: NodeReplExecuteInput) => Promise<NodeReplRunResult>;
export interface NodeReplMcpRuntime {
    dispose(): void;
    server: Server;
}
export declare function setNodeReplMcpProcessTitle(target?: {
    title: string;
}): void;
/**
* The test and the same process embedding entry use the same execution logic; the production stdio calls it in a one-time Worker by default.
* Thus, even Node's module cache is destroyed along with the call.
 */
export declare function createInProcessNodeReplExecutor(): NodeReplExecutor;
export declare function createNodeReplMcpRuntime(input?: {
    executeJs?: NodeReplExecutor;
    cuaRuntime?: ComputerUseRuntime;
}): NodeReplMcpRuntime;
export { installNodeReplProcessGuards, installNodeReplShutdownTriggers };
export declare function main(): Promise<void>;
export declare function captureComputerUseRuntimeFromEnvironment(env?: NodeJS.ProcessEnv): ComputerUseRuntime | undefined;
//# sourceMappingURL=server.d.ts.map