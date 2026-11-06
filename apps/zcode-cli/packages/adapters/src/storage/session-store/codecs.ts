import {
  SESSION_TASK_TYPES,
  SESSION_ENTRY_MODEL_SELECTION,
  SESSION_TITLE_SOURCES,
  parseModelSelectionValue,
  type CollaborationMode,
  type FileDiff,
  type MessageId,
  type MessageInfo,
  type MessagePart,
  type PartId,
  type PermissionRuleset,
  type ProjectId,
  type SessionEntryInfo,
  type SessionId,
  type SessionInfo,
  type SessionRevert,
  type SessionTitleSource,
  type SessionEntryType,
  type TodoItem,
  type TraceId,
  type WorkspaceId,
  type SessionTaskType,
} from "@zcode/contracts";
import { decodeJson } from "./json.js";
import type { MessageRow, PartRow, SessionEntryRow, SessionRow, TodoRow } from "./rows.js";

export function isCollaborationMode(value: unknown): value is CollaborationMode {
  return (
    value === "plan" ||
    value === "build" ||
    value === "edit" ||
    value === "yolo" ||
    value === "auto"
  );
}

function decodeSessionTaskType(value: string | null | undefined): SessionTaskType {
  return SESSION_TASK_TYPES.includes(value as SessionTaskType)
    ? (value as SessionTaskType)
    : "interactive";
}

function decodeSessionTitleSource(value: string | null | undefined): SessionTitleSource {
  return SESSION_TITLE_SOURCES.includes(value as SessionTitleSource)
    ? (value as SessionTitleSource)
    : "first_input";
}

export function decodeSessionRow(row: SessionRow): SessionInfo {
  return {
    id: row.id as SessionId,
    projectID: row.project_id as ProjectId,
    workspaceID: row.workspace_id ? (row.workspace_id as WorkspaceId) : undefined,
    parentID: row.parent_id ? (row.parent_id as SessionId) : undefined,
    traceID: row.trace_id ? (row.trace_id as TraceId) : undefined,
    taskType: decodeSessionTaskType(row.task_type),
    slug: row.slug,
    directory: row.directory,
    path: row.path ?? undefined,
    title: row.title,
    titleSource: decodeSessionTitleSource(row.title_source),
    titleMessageID: row.title_message_id ? (row.title_message_id as MessageId) : undefined,
    version: row.version,
    shareURL: row.share_url ?? undefined,
    summaryAdditions: row.summary_additions ?? undefined,
    summaryDeletions: row.summary_deletions ?? undefined,
    summaryFiles: row.summary_files ?? undefined,
    summaryDiffs: decodeJson<FileDiff[]>(row.summary_diffs),
    revert: decodeJson<SessionRevert>(row.revert),
    permission: decodeJson<PermissionRuleset>(row.permission),
    time: {
      created: row.time_created,
      updated: row.time_updated,
      titleUpdated: row.time_title_updated ?? undefined,
      compacting: row.time_compacting ?? undefined,
      archived: row.time_archived ?? undefined,
    },
  };
}

export function decodeMessageRow(row: MessageRow): MessageInfo {
  return {
    ...decodeStoredMessage(JSON.parse(row.data) as unknown),
    id: row.id as MessageId,
    sessionID: row.session_id as SessionId,
  } as MessageInfo;
}

function decodeStoredMessage(value: unknown): Record<string, unknown> {
  if (!isRecord(value)) return {};
  if (value.role === "user") {
    const { model: _legacyModel, modelSelection: rawSelection, ...message } = value;
// Migration may leave null when the identity cannot be determined; fields may also be missing after rollback. An incomplete configuration cannot block protocol reading of the entire message.
// Only the new structure is verified here, execution qualification is not checked, old snapshots are not read back, and the disk content remains unchanged.
    const modelSelection = parseModelSelectionValue(rawSelection);
    return { ...message, ...(modelSelection ? { modelSelection } : {}) };
  }
  if (value.role === "assistant") {
    const {
      providerID: _legacyProviderId,
      modelID: _legacyModelId,
      variant: _legacyReasoningLevel,
      ...message
    } = value;
    return message;
  }
  return value;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function decodePartRow(row: PartRow): MessagePart {
  return {
    ...decodeStoredPart(JSON.parse(row.data) as unknown),
    id: row.id as PartId,
    sessionID: row.session_id as SessionId,
    messageID: row.message_id as MessageId,
  } as MessagePart;
}

function decodeStoredPart(value: unknown): Record<string, unknown> {
  if (!isRecord(value)) return {};
  if (value.type === "timeline" && value.timelineType === "model_change") {
    const {
      fromModel: _oldFrom,
      toModel: _oldTo,
      fromModelSelection,
      toModelSelection,
      ...part
    } = value;
    const fromModel = decodeTimelineSelection(fromModelSelection);
    const toModel = decodeTimelineSelection(toModelSelection);
    return {
      ...part,
      ...(fromModel ? { fromModel } : {}),
      ...(toModel ? { toModel } : {}),
    };
  }
  if (value.type === "subtask") {
    const { model: _oldModel, modelSelection, ...part } = value;
    const model = parseModelSelectionValue(modelSelection);
    return { ...part, ...(model ? { model } : {}) };
  }
  return value;
}

function decodeTimelineSelection(value: unknown) {
  if (!isRecord(value)) return undefined;
// Label is only Timeline display information and does not belong to strict Selection; legal labeled history cannot be deleted by mistake.
  const { label, ...rawSelection } = value;
  const selection = parseModelSelectionValue(rawSelection);
  return selection ? { ...selection, ...(typeof label === "string" ? { label } : {}) } : undefined;
}

export function decodeSessionEntryRow(row: SessionEntryRow): SessionEntryInfo {
  const rawData = JSON.parse(row.data) as unknown;
  return {
    id: row.id,
    sessionID: row.session_id as SessionId,
    type: row.type as SessionEntryType | string,
    time: {
      created: row.time_created,
      updated: row.time_updated,
    },
    data:
      row.type === SESSION_ENTRY_MODEL_SELECTION
        ? decodeStoredSessionModelSelection(rawData)
        : rawData,
  };
}

function decodeStoredSessionModelSelection(value: unknown): unknown {
//Old tile fields only belong to the upgrade entry. Even if the new member is empty/illegal, it cannot borrow the value from the old snapshot.
// Unpacking is completed at the storage boundary, core/bootstrap/fork only consumes the current logical Selection of the port.
  if (!isRecord(value)) return undefined;
  return parseModelSelectionValue(value.modelSelection) ?? value.modelSelection;
}

export function decodeTodoRow(row: TodoRow): TodoItem {
  return {
    content: row.content,
    status: row.status as TodoItem["status"],
    priority: row.priority as TodoItem["priority"],
  };
}

export function partCreatedAt(part: MessagePart, fallback: number): number {
  if (part.type === "text" || part.type === "reasoning") return part.time?.start ?? fallback;
  if (part.type === "compaction") return part.time?.start ?? fallback;
  if (part.type === "timeline") return part.time?.start ?? fallback;
  if (part.type === "tool") {
    if (part.state.status === "running") return part.state.time.start;
    if (part.state.status === "completed" || part.state.status === "error") {
      return part.state.time.start;
    }
  }
  if (part.type === "retry") return part.time.created;
  return fallback;
}
