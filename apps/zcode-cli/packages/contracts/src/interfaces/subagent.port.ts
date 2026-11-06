// ============================================================
// Subagent Port - child agent execution boundary
// ============================================================

import type { AgentBackgroundedOutput, AgentOutput } from "../tools/agent.js";
import type { Model, ModelSelection } from "../model/index.js";
import type { ModelRequestDependencies } from "../model/invocation-context.js";
import type { SessionId, ToolCallId, TurnId } from "./shared.js";
import type { TraceContext } from "../tracing/tracer.js";

export interface SubagentRunRequest {
  sessionId: SessionId;
  turnId?: TurnId;
  parentToolCallId: ToolCallId | string;
  agentType: string;
  description: string;
  prompt: string;
  callerCanReadOutputFile?: boolean;
  workingDirectory: string;
  workspaceRoot: string;
  trace: TraceContext;
}

export interface SubagentRunOptions {
  signal?: AbortSignal;
/** The immutable Model inherited from the parent Agent Loop by a child that has not explicitly selected a model. */
  model?: Model;
/** Core Server’s highest priority Selection for the foreground child; each child still creates its own Model. */
  modelOverride?: {
    selection: ModelSelection;
    requestDependencies?: ModelRequestDependencies;
    background: "deny";
  };
}

export interface SubagentLaunchRequest extends SubagentRunRequest {
  runInBackground?: boolean;
}

export type SubagentLaunchOptions = SubagentRunOptions;

export type SubagentStartRequest = SubagentRunRequest;

export interface SubagentStartOptions {
  signal?: AbortSignal;
/** The ordinary Model inherited when the background child starts; the temporary turn model is still prohibited from entering the background. */
  model?: Model;
}

export interface SubagentWaitOptions {
  signal?: AbortSignal;
}

export interface SubagentStopOptions {
  signal?: AbortSignal;
}

export interface SubagentSendMessageRequest {
  sessionId: SessionId;
  turnId?: TurnId;
  parentToolCallId: ToolCallId | string;
  to: string;
  summary: string;
  message: string;
  workingDirectory: string;
  workspaceRoot: string;
  trace: TraceContext;
}

export interface SubagentSendMessageOptions {
  signal?: AbortSignal;
}

export type SubagentSendMessageDelivery = "queued" | "steered" | "resumed_background";

export interface SubagentSendMessageResult {
  status: "success" | "failed";
  messageId: string;
  delivery?: SubagentSendMessageDelivery;
  message?: string;
  error?: string;
  agentId?: string;
  taskId?: string;
  outputFile?: string;
}

export type SubagentTaskStatus =
  | "running"
  | "completed"
  | "failed"
  | "cancelled"
  | "killed"
  | "stopped"
  | "lost";

export interface SubagentTaskSnapshot {
  taskId: string;
  agentId: string;
  agentType: string;
  description: string;
  status: SubagentTaskStatus;
  startedAt: Date;
  completedAt?: Date;
  childSessionId?: SessionId;
  parentToolCallId?: ToolCallId | string;
  pid?: number;
  error?: string;
  output?: AgentOutput;
  outputFile?: string;
  notified?: boolean;
}

export interface SubagentPort {
  launch(request: SubagentLaunchRequest, options?: SubagentLaunchOptions): Promise<AgentOutput>;
  run(request: SubagentRunRequest, options?: SubagentRunOptions): Promise<AgentOutput>;
  start?(
    request: SubagentStartRequest,
    options?: SubagentStartOptions,
  ): Promise<AgentBackgroundedOutput>;
  backgroundTask?(taskId: string): Promise<SubagentTaskSnapshot | undefined>;
  getTask?(taskId: string): Promise<SubagentTaskSnapshot | undefined>;
  waitForTask?(
    taskId: string,
    options?: SubagentWaitOptions,
  ): Promise<SubagentTaskSnapshot | undefined>;
  stopTask?(
    taskId: string,
    options?: SubagentStopOptions,
  ): Promise<SubagentTaskSnapshot | undefined>;
  sendMessage?(
    request: SubagentSendMessageRequest,
    options?: SubagentSendMessageOptions,
  ): Promise<SubagentSendMessageResult>;
}
