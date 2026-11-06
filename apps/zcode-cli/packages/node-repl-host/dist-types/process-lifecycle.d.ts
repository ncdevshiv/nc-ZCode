/**
* REPL cell's synchronization errors are handled by NodeReplSession, but fire-and-forget's asynchronous errors
* (If the tab.* call without await is rejected when the turn is interrupted), the entire server will be penetrated according to Node's default strategy.
* Process. Once the child process dies, Browser Use within the session is no longer available. Here the asynchronous errors are downgraded to stderr logs:
* Runtime state is preserved and protocol stdout is not affected.
 *
* After the parent process exits, stderr will report EPIPE. The old handler writes back the same EPIPE stack
* stderr, forming an infinite loop of EPIPE -> uncaughtException -> stderr.write -> EPIPE.
* Closing the output pipe means that the MCP client is unreachable and must enter shutdown directly, and cannot continue to write diagnostics.
 */
export declare function installNodeReplProcessGuards(input: {
    onOutputClosed: (error: Error) => void;
    process: Pick<NodeJS.Process, "on">;
    writeStderr: (text: string) => void;
}): void;
export declare function installNodeReplShutdownTriggers(input: {
    process: Pick<NodeJS.Process, "once">;
    shutdown: () => void;
    stdin: Pick<NodeJS.ReadStream, "once">;
}): void;
export declare function isDirectMcpEntrypoint(importMetaUrl: string, argvPath: string | undefined): Promise<boolean>;
//# sourceMappingURL=process-lifecycle.d.ts.map