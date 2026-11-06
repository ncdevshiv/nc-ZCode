/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { createHash } from "node:crypto";
import { isAbsolute, resolve } from "node:path";
import { nativeApplyPatch, type StructuredPatch } from "@zcode/sidecar";
import {
  RewindScope,
  RewindStrategy,
  SessionEventType,
  getCurrentTraceContext,
  isFileSystemPortError,
  parseWorkspaceCheckpointArtifact,
  traceContextToLogContext,
} from "../deps.js";
import type {
  CheckpointCreatedPayload,
  MessageId,
  TraceContext,
  TurnId,
  WorkspaceCheckpointArtifact,
} from "../deps.js";
import {
  selectCheckpointForRewind,
  selectCheckpointsForMessages,
  throwIfTurnAborted,
} from "../helpers/index.js";
import type {
  WorkspaceFileRewindApplyResult,
  WorkspaceFileRewindIgnoredFile,
  WorkspaceFileRewindPreview,
  WorkspaceFileRewindSafeFile,
  WorkspaceFileRewindUnsafeFile,
  WorkspaceFileRewindUnsafeReason,
} from "../types.js";
import type { AgentRuntimeInternal } from "../internal.js";

type PlannedFileState = {
  content: string | null;
  exists: boolean;
  hash: string | null;
};

type FileCheckpointOperation = {
  action: "restore" | "delete";
  afterContent: string | null;
  artifact: WorkspaceCheckpointArtifact;
  beforeContent: string | null;
  checkpoint: CheckpointCreatedPayload;
  path: string;
  toolName: string;
};

type WorkspaceFileRewindPlan = WorkspaceFileRewindPreview & {
  operations: FileCheckpointOperation[];
};

type FileRewindJournalEntry = {
  path: string;
  state: PlannedFileState;
};

interface FileAggregate {
  action: "restore" | "delete";
  operationCount: number;
  path: string;
  toolNames: Set<string>;
  unsafe?: {
    currentHash?: string;
    expectedHash?: string;
    message?: string;
    reason: WorkspaceFileRewindUnsafeReason;
  };
}

interface IgnoredFileAggregate {
  operationCount: number;
  path: string;
  toolNames: Set<string>;
}

export async function previewWorkspaceFileRewind(
  this: AgentRuntimeInternal,
  options: {
    abortSignal?: AbortSignal;
    targetCheckpointId?: string;
    targetMessageId?: MessageId;
    targetMessageIds?: MessageId[];
    targetTurnId?: TurnId;
    traceContext?: TraceContext;
  } = {},
): Promise<WorkspaceFileRewindPreview> {
  const plan = await buildWorkspaceFileRewindPlan.call(this, options);
  return toPreview(plan);
}

export async function applyWorkspaceFileRewind(
  this: AgentRuntimeInternal,
  options: {
    abortSignal?: AbortSignal;
    targetCheckpointId?: string;
    targetMessageId?: MessageId;
    targetMessageIds?: MessageId[];
    targetTurnId?: TurnId;
    traceContext?: TraceContext;
/** Combined rewind submission gate: Submit branch cut after all files are written successfully and before the workspace event is released. */
    commitAfterApply?: () => Promise<void>;
  } = {},
): Promise<WorkspaceFileRewindApplyResult> {
  const traceContext =
    options.traceContext ?? getCurrentTraceContext() ?? this.rootTraceContext;
  const plan = await buildWorkspaceFileRewindPlan.call(this, {
    ...options,
    traceContext,
  });
  if (!plan.canApply) {
    return {
      applied: false,
      preview: toPreview(plan),
      response: "File rewind was not applied because at least one file is unsafe.",
    };
  }

  if (!this.fileSystemPort) {
    return {
      applied: false,
      preview: toPreview({
        ...plan,
        canApply: false,
        unsafeFiles: [
          ...plan.unsafeFiles,
          {
            operationCount: 1,
            path: "workspace",
            reason: "file_read_failed",
            toolNames: [],
            message: "FileSystemPort is not configured.",
          },
        ],
      }),
      response: "File rewind was not applied because the file-system adapter is unavailable.",
    };
  }

  const restoredFiles: Array<{ action: "delete" | "restore"; path: string }> = [];
  const journal: FileRewindJournalEntry[] = [];
  try {
    for (const operation of plan.operations) {
      throwIfTurnAborted(options.abortSignal);
      const state = await readCurrentFileState.call(
        this,
        operation.path,
        traceContext,
        options.abortSignal,
      );
      if ("reason" in state) {
        throw new Error(state.message ?? `Failed to journal ${operation.path}`);
      }
      journal.push({ path: operation.path, state });
      if (operation.action === "delete" || operation.beforeContent === null) {
        await this.fileSystemPort.removeFile(
          {
            path: operation.path,
            missingOk: true,
            trace: traceContext,
          },
          { signal: options.abortSignal },
        );
        restoredFiles.push({ action: "delete", path: operation.path });
        continue;
      }

      await this.fileSystemPort.writeTextFile(
        {
          path: operation.path,
          content: operation.beforeContent,
          createParents: true,
          atomic: true,
          trace: traceContext,
        },
        { signal: options.abortSignal },
      );
      restoredFiles.push({ action: "restore", path: operation.path });
    }
    await options.commitAfterApply?.();
  } catch (error) {
// Multi-file rewind used to leave a semi-rollback workspace when the Nth write failed.
// The journal restores the contents before command execution in the reverse order of writing; compensation failure is upgraded to an unrecoverable error.
    try {
      await compensateFileRewindJournal.call(this, journal, traceContext);
    } catch (compensationError) {
      throw new AggregateError(
        [error, compensationError],
        "Workspace file rewind failed and compensation was incomplete",
      );
    }
    const message = error instanceof Error ? error.message : String(error);
    return {
      applied: false,
      preview: {
        ...toPreview(plan),
        canApply: false,
        unsafeFiles: [
          ...plan.unsafeFiles,
          {
            operationCount: Math.max(1, journal.length),
            path: journal.at(-1)?.path ?? "workspace",
            reason: "file_read_failed",
            toolNames: [],
            message,
          },
        ],
      },
      response: "File rewind was not applied because the write failed; prior files were restored.",
    };
  }

  const firstOperation = plan.operations[0];
  const lastOperation = plan.operations.at(-1);
  const targetMessageId = options.targetMessageId ?? options.targetMessageIds?.[0];
  const rewindId = `rewind_${crypto.randomUUID()}`;
  const event = this.createEvent(
    SessionEventType.RewindTriggered,
    {
      rewindId,
      scope: RewindScope.Workspace,
      strategy: RewindStrategy.ActiveChain,
      targetMessageId,
      targetCheckpointId: lastOperation?.checkpoint.checkpointId,
      restoredSnapshotRef: lastOperation?.checkpoint.snapshotRef,
      reason: "file_summary_rewind",
    },
    traceContext,
  );
  await this.appendEvent(event, traceContext);

  this.logger?.info("Workspace file summary rewind applied", {
    ...traceContextToLogContext(traceContext),
    event: "rewind.file_summary.completed",
    fileCount: plan.safeFiles.length,
    module: "core.runtime",
    operationCount: plan.operations.length,
    restoredFileCount: restoredFiles.length,
    status: "completed",
    targetCheckpointId: lastOperation?.checkpoint.checkpointId,
    targetMessageId,
  });

  return {
    applied: true,
    preview: toPreview(plan),
    response: `Rewound ${plan.safeFiles.length} file${plan.safeFiles.length === 1 ? "" : "s"} from summary checkpoints.`,
  };
}

async function compensateFileRewindJournal(
  this: AgentRuntimeInternal,
  journal: FileRewindJournalEntry[],
  traceContext: TraceContext,
): Promise<void> {
  for (const entry of [...journal].reverse()) {
    if (!entry.state.exists || entry.state.content === null) {
      await this.fileSystemPort!.removeFile({
        path: entry.path,
        missingOk: true,
        trace: traceContext,
      });
      continue;
    }
    await this.fileSystemPort!.writeTextFile({
      path: entry.path,
      content: entry.state.content,
      createParents: true,
      atomic: true,
      trace: traceContext,
    });
  }
}

async function buildWorkspaceFileRewindPlan(
  this: AgentRuntimeInternal,
  options: {
    abortSignal?: AbortSignal;
    targetCheckpointId?: string;
    targetMessageId?: MessageId;
    targetMessageIds?: MessageId[];
    targetTurnId?: TurnId;
    traceContext?: TraceContext;
  },
): Promise<WorkspaceFileRewindPlan> {
  const traceContext =
    options.traceContext ?? getCurrentTraceContext() ?? this.rootTraceContext;
  if (!this.artifactStore || !this.fileSystemPort) {
    return {
      canApply: false,
      ignoredFiles: [],
      operations: [],
      safeFiles: [],
      unsafeFiles: [
        {
          operationCount: 1,
          path: "workspace",
          reason: !this.artifactStore ? "checkpoint_unreadable" : "file_read_failed",
          toolNames: [],
          message: !this.artifactStore
            ? "ArtifactStore is not configured."
            : "FileSystemPort is not configured.",
        },
      ],
    };
  }

  const events = await this.eventStore.getEvents(this.sessionId);
  const checkpoints = resolveTargetCheckpoints(events, {
    targetCheckpointId: options.targetCheckpointId,
    targetMessageId: options.targetMessageId,
    targetMessageIds: options.targetMessageIds,
  });
  if (checkpoints.length === 0) {
    return {
      canApply: false,
      ignoredFiles: [],
      operations: [],
      safeFiles: [],
      unsafeFiles: [],
    };
  }

  const artifacts: Array<{
    artifact: WorkspaceCheckpointArtifact;
    checkpoint: CheckpointCreatedPayload;
  }> = [];
  const unreadableFiles: WorkspaceFileRewindUnsafeFile[] = [];

  for (const checkpoint of checkpoints) {
    throwIfTurnAborted(options.abortSignal);
    try {
      const read = await this.artifactStore.readToolResultArtifact(
        {
          uri: checkpoint.snapshotRef,
          trace: traceContext,
        },
        { signal: options.abortSignal },
      );
      artifacts.push({
        artifact: parseWorkspaceCheckpointArtifact(JSON.parse(read.content)),
        checkpoint,
      });
    } catch (error) {
      unreadableFiles.push({
        operationCount: Math.max(1, checkpoint.fileCount ?? 1),
        path: `checkpoint:${checkpoint.checkpointId}`,
        reason: "checkpoint_unreadable",
        toolNames: [],
        message: error instanceof Error ? error.message : String(error),
      });
    }
  }

  const ignoredByPath = new Map<string, IgnoredFileAggregate>();
  const operations: FileCheckpointOperation[] = [];
  const unsupportedByPath = new Map<string, FileAggregate>();

  for (const { artifact, checkpoint } of artifacts) {
    if (isIgnoredShellTool(artifact.toolName)) {
      for (const file of artifact.files) {
        addIgnoredFile(
          ignoredByPath,
          resolveCheckpointFilePath(this.workspaceRoot, file.path),
          artifact.toolName,
        );
      }
      continue;
    }

    for (const file of artifact.files) {
// Write/ApplyPatch checkpoint will retain the relative path of the workspace passed in by the model, and
// FileSystemPort only accepts absolute paths. If not in the planning stage, press runtime workspace root
// Parsing, safe files will be falsely reported as file_read_failed, and combined rewind will only return blocked.
      const filePath = resolveCheckpointFilePath(this.workspaceRoot, file.path);
      const afterContent = await resolveCheckpointAfterContent(file);
      if (afterContent === undefined) {
        markUnsafe(unsupportedByPath, {
          action: file.existedBefore && file.beforeContent !== null ? "restore" : "delete",
          path: filePath,
          reason: "unsupported_checkpoint",
          toolName: artifact.toolName,
        });
        continue;
      }

      operations.push({
        action: file.existedBefore && file.beforeContent !== null ? "restore" : "delete",
        afterContent,
        artifact,
        beforeContent: file.beforeContent,
        checkpoint,
        path: filePath,
        toolName: artifact.toolName,
      });
    }
  }

  const simulatedByPath = new Map<string, PlannedFileState>();
  const safeByPath = new Map<string, FileAggregate>();
  const unsafeByPath = new Map<string, FileAggregate>(unsupportedByPath);
  const applyOperations: FileCheckpointOperation[] = [];

  for (const operation of [...operations].reverse()) {
    throwIfTurnAborted(options.abortSignal);
    const aggregate = ensureFileAggregate(safeByPath, operation);
    aggregate.operationCount += 1;
    aggregate.toolNames.add(operation.toolName);
    aggregate.action = operation.action;

    if (unsafeByPath.has(operation.path)) {
      continue;
    }

    const currentState =
      simulatedByPath.get(operation.path) ??
      (await readCurrentFileState.call(this, operation.path, traceContext, options.abortSignal));
    if ("reason" in currentState) {
      markUnsafe(unsafeByPath, {
        action: operation.action,
        message: currentState.message,
        path: operation.path,
        reason: currentState.reason,
        toolName: operation.toolName,
      });
      continue;
    }

    const expectedHash = hashContent(operation.afterContent);
    if (currentState.hash !== expectedHash) {
      markUnsafe(unsafeByPath, {
        action: operation.action,
        currentHash: currentState.hash ?? "missing",
        expectedHash,
        path: operation.path,
        reason: "external_modified",
        toolName: operation.toolName,
      });
      continue;
    }

    simulatedByPath.set(operation.path, {
      content: operation.beforeContent,
      exists: operation.beforeContent !== null,
      hash: hashContent(operation.beforeContent),
    });
    applyOperations.push(operation);
  }

  for (const [path, unsafe] of unsafeByPath) {
    safeByPath.delete(path);
    unsafe.operationCount = Math.max(
      unsafe.operationCount,
      operations.filter((operation) => operation.path === path).length,
    );
  }

  const safeFiles = Array.from(safeByPath.values()).map(toSafeFile).sort(compareByPath);
  const unsafeFiles = [
    ...unreadableFiles,
    ...Array.from(unsafeByPath.values()).map(toUnsafeFile),
  ].sort(compareByPath);
  const ignoredFiles = Array.from(ignoredByPath.values()).map(toIgnoredFile).sort(compareByPath);

  return {
    canApply: safeFiles.length > 0 && unsafeFiles.length === 0,
    ignoredFiles,
    operations: unsafeFiles.length === 0 ? applyOperations : [],
    safeFiles,
    unsafeFiles,
  };
}

function resolveCheckpointFilePath(workspaceRoot: string, path: string): string {
  return isAbsolute(path) ? path : resolve(workspaceRoot, path);
}

function resolveTargetCheckpoints(
  events: Parameters<typeof selectCheckpointForRewind>[0],
  target: {
    targetCheckpointId?: string;
    targetMessageId?: MessageId;
    targetMessageIds?: MessageId[];
    targetTurnId?: TurnId;
  },
): CheckpointCreatedPayload[] {
  const targetMessageIds =
    target.targetMessageIds && target.targetMessageIds.length > 0
      ? target.targetMessageIds
      : target.targetMessageId
        ? [target.targetMessageId]
        : [];
  if (targetMessageIds.length > 0) {
    const checkpoints = selectCheckpointsForMessages(events, targetMessageIds);
    if (checkpoints.length > 0 || !target.targetTurnId) {
      return checkpoints;
    }
  }

  if (target.targetTurnId && !String(target.targetTurnId).includes("~")) {
// The old TurnStarted does not have user messageId and cannot pass rowId->messageId
// Find the workspace checkpoint; common turn available events turnId accurate. split
// The product turn has the "~q" suffix and cannot be used as the runtime turnId to avoid being linked to other queued turns.
    return events
      .filter((event) => event.type === SessionEventType.CheckpointCreated)
      .filter((event) => String(event.turnId ?? "") === String(target.targetTurnId))
      .map((event) => event.payload)
      .filter((payload): payload is CheckpointCreatedPayload => {
        const checkpoint = payload as Partial<CheckpointCreatedPayload>;
        return (
          checkpoint.scope === RewindScope.Workspace ||
          checkpoint.scope === RewindScope.Both
        );
      });
  }

  const checkpoint = selectCheckpointForRewind(events, target.targetCheckpointId);
  return checkpoint ? [checkpoint] : [];
}

async function resolveCheckpointAfterContent(
  file: WorkspaceCheckpointArtifact["files"][number],
): Promise<string | null | undefined> {
  const afterContent = (file as { afterContent?: unknown }).afterContent;
  if (typeof afterContent === "string") {
    return afterContent;
  }

  if (!file.existedBefore && file.beforeContent === null && file.structuredPatch.length === 0) {
    return undefined;
  }

  const beforeContent = file.beforeContent ?? "";
  const patch: StructuredPatch = {
    oldFileName: file.path,
    newFileName: file.path,
    oldHeader: undefined,
    newHeader: undefined,
    hunks: file.structuredPatch,
  };
  const patched = await nativeApplyPatch(beforeContent, patch, {
    autoConvertLineEndings: false,
    fuzzFactor: 0,
  });
  return typeof patched === "string" ? patched : undefined;
}

async function readCurrentFileState(
  this: AgentRuntimeInternal,
  path: string,
  traceContext: TraceContext,
  abortSignal: AbortSignal | undefined,
): Promise<PlannedFileState | { message?: string; reason: "file_read_failed" }> {
  try {
    const read = await this.fileSystemPort!.readTextFile(
      {
        path,
        trace: traceContext,
      },
      { signal: abortSignal },
    );
    return {
      content: read.content,
      exists: true,
      hash: hashContent(read.content),
    };
  } catch (error) {
    if (isFileSystemPortError(error) && error.code === "not_found") {
      return {
        content: null,
        exists: false,
        hash: hashContent(null),
      };
    }
    return {
      reason: "file_read_failed",
      message: error instanceof Error ? error.message : String(error),
    };
  }
}

function hashContent(content: string | null): string {
  if (content === null) {
    return "missing";
  }
  return createHash("sha256").update(content).digest("hex");
}

function isIgnoredShellTool(toolName: string): boolean {
  const normalized = toolName.toLowerCase().replace(/[^a-z0-9]/g, "");
  return (
    normalized === "bash" ||
    normalized === "shell" ||
    normalized.includes("terminal") ||
    normalized.endsWith("shell")
  );
}

function ensureFileAggregate(
  aggregates: Map<string, FileAggregate>,
  operation: FileCheckpointOperation,
): FileAggregate {
  const existing = aggregates.get(operation.path);
  if (existing) {
    return existing;
  }
  const aggregate: FileAggregate = {
    action: operation.action,
    operationCount: 0,
    path: operation.path,
    toolNames: new Set(),
  };
  aggregates.set(operation.path, aggregate);
  return aggregate;
}

function markUnsafe(
  aggregates: Map<string, FileAggregate>,
  input: {
    action: "restore" | "delete";
    currentHash?: string;
    expectedHash?: string;
    message?: string;
    path: string;
    reason: WorkspaceFileRewindUnsafeReason;
    toolName: string;
  },
): void {
  const existing = aggregates.get(input.path);
  if (existing) {
    existing.operationCount += 1;
    existing.toolNames.add(input.toolName);
    existing.unsafe = {
      currentHash: input.currentHash ?? existing.unsafe?.currentHash,
      expectedHash: input.expectedHash ?? existing.unsafe?.expectedHash,
      message: input.message ?? existing.unsafe?.message,
      reason: existing.unsafe?.reason ?? input.reason,
    };
    return;
  }

  aggregates.set(input.path, {
    action: input.action,
    operationCount: 1,
    path: input.path,
    toolNames: new Set([input.toolName]),
    unsafe: {
      currentHash: input.currentHash,
      expectedHash: input.expectedHash,
      message: input.message,
      reason: input.reason,
    },
  });
}

function addIgnoredFile(
  aggregates: Map<string, IgnoredFileAggregate>,
  path: string,
  toolName: string,
): void {
  const existing = aggregates.get(path);
  if (existing) {
    existing.operationCount += 1;
    existing.toolNames.add(toolName);
    return;
  }
  aggregates.set(path, {
    operationCount: 1,
    path,
    toolNames: new Set([toolName]),
  });
}

function toSafeFile(file: FileAggregate): WorkspaceFileRewindSafeFile {
  return {
    action: file.action,
    operationCount: file.operationCount,
    path: file.path,
    toolNames: Array.from(file.toolNames).sort(),
  };
}

function toUnsafeFile(file: FileAggregate): WorkspaceFileRewindUnsafeFile {
  return {
    operationCount: file.operationCount,
    path: file.path,
    reason: file.unsafe?.reason ?? "unsupported_checkpoint",
    toolNames: Array.from(file.toolNames).sort(),
    ...(file.unsafe?.message ? { message: file.unsafe.message } : {}),
    ...(file.unsafe?.expectedHash ? { expectedHash: file.unsafe.expectedHash } : {}),
    ...(file.unsafe?.currentHash ? { currentHash: file.unsafe.currentHash } : {}),
  };
}

function toIgnoredFile(file: IgnoredFileAggregate): WorkspaceFileRewindIgnoredFile {
  return {
    operationCount: file.operationCount,
    path: file.path,
    reason: "bash_ignored",
    toolNames: Array.from(file.toolNames).sort(),
  };
}

function compareByPath<T extends { path: string }>(left: T, right: T): number {
  return left.path.localeCompare(right.path);
}

function toPreview(plan: WorkspaceFileRewindPlan): WorkspaceFileRewindPreview {
  return {
    canApply: plan.canApply,
    ignoredFiles: plan.ignoredFiles,
    safeFiles: plan.safeFiles,
    unsafeFiles: plan.unsafeFiles,
  };
}
