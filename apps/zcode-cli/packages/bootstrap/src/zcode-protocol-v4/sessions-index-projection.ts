// CLI-side reduction of sessions-index topic: Reduce the contents of each session under a workspace name
// ConversationSnapshot derives from SessionSummary and maintains the conflated latest state + outputs upsert/remove delta.
// Pure reduction, no IO; publisher (sessions-index-publisher) is responsible for seq accounting and frame construction.
//Event subscription and flush scheduling return to gateway (v4-gateway).
import {
  deriveSessionWorkflowActivity,
  type ConversationSnapshot,
  type SessionSummary,
  type SessionsIndexDelta,
  type SessionsIndexSnapshot,
} from "@zcode/shared/zcode-protocol-v4";

/** Derive a summary that requires session-level meta-information beyond the snapshot (from session-store record/event time). */
export interface SessionSummaryDeriveExtra {
  workspaceId: string;
  createdAt: number;
  lastActivityAt: number;
  parentSessionId?: string;
}

const MAX_PREVIEW_CHARS = 120;

/** Derive SessionSummary (pure function, golden testable) from ConversationSnapshot + session meta information. */
function deriveSessionSummary(
  snapshot: ConversationSnapshot,
  extra: SessionSummaryDeriveExtra,
): SessionSummary {
//The text of the last assistantText row is previewed (≤120 characters).
  let lastAssistantPreview: string | undefined;
  const rows = snapshot.rows.window;
  for (let i = rows.length - 1; i >= 0; i -= 1) {
    const row = rows[i];
    if (
      !lastAssistantPreview &&
      row &&
      row.kind === "assistantText" &&
      row.text.trim().length > 0
    ) {
      lastAssistantPreview = row.text.slice(0, MAX_PREVIEW_CHARS);
    }
    if (lastAssistantPreview) break;
  }
  const hasBackgroundWork = snapshot.backgroundWorks.some((work) => work.status === "running");
// Data for sidebar workflow running rows:
// Derived from workflowRuns + backgroundWorks of the same snapshot, the sidebar does not need to subscribe to the run progress.
  const workflowActivity = deriveSessionWorkflowActivity({
    workflowRuns: snapshot.workflowRuns,
    backgroundWorks: snapshot.backgroundWorks,
  });
// workspaceHookReview is rendered by Hooks Settings and does not degrade to the permission/userInput sidebar logo.
  const pending = snapshot.pendingInteractions.find(
    (interaction) => interaction.kind === "permission" || interaction.kind === "userInput",
  );
  const permissionCount = snapshot.pendingInteractions.filter(
    (interaction) => interaction.kind === "permission",
  ).length;
  const userInputCount = snapshot.pendingInteractions.filter(
    (interaction) => interaction.kind === "userInput",
  ).length;
// Compatible with the cropped snapshot of the restoration migration or reduction test structure: it cannot be used due to the new lightweight toolName projection.
//Block the entire task list when the old pending interaction is temporarily missing the payload.
  const pendingPayload = pending?.payload;
  const pendingToolName =
    pendingPayload && "toolName" in pendingPayload ? pendingPayload.toolName : undefined;
  const pendingInteraction =
    pending &&
    pendingPayload &&
    (pendingPayload.kind === "permission" || pendingPayload.kind === "userInput")
      ? {
          interactionId: pending.interactionId,
          kind: pendingPayload.kind,
          ...(pendingToolName ? { toolName: pendingToolName } : {}),
          ...(pending.autoResolution ? { autoResolution: pending.autoResolution } : {}),
        }
      : undefined;
  return {
    sessionId: snapshot.sessionId,
    workspaceId: extra.workspaceId,
    ...(extra.parentSessionId ? { parentSessionId: extra.parentSessionId } : {}),
    title: snapshot.meta.title,
    titleSource: snapshot.meta.titleSource,
    phase: snapshot.control.phase,
// Faithful transparent transmission control.sessionEnded (semantics: true after successful round closing, does not mean deleted).
    sessionEnded: snapshot.control.sessionEnded,
    hasBackgroundWork,
    ...(workflowActivity === undefined ? {} : { workflowActivity }),
    ...(pendingInteraction ? { pendingInteraction } : {}),
    ...(permissionCount > 0 || userInputCount > 0
      ? {
          pendingInteractionSummary: {
            permissionCount,
            userInputCount,
          },
        }
      : {}),
    ...(snapshot.goal ? { goalStatus: snapshot.goal.status } : {}),
    lastActivityAt: extra.lastActivityAt,
    ...(lastAssistantPreview ? { lastAssistantPreview } : {}),
    createdAt: extra.createdAt,
  };
}

/** Whether two summaries are equivalent (conflation: if they are equivalent, delta will not be generated). */
function summariesEqual(a: SessionSummary, b: SessionSummary): boolean {
  return (
    a.sessionId === b.sessionId &&
    a.workspaceId === b.workspaceId &&
    a.parentSessionId === b.parentSessionId &&
    a.title === b.title &&
    a.titleSource === b.titleSource &&
    a.phase === b.phase &&
    a.sessionEnded === b.sessionEnded &&
    a.hasBackgroundWork === b.hasBackgroundWork &&
// phase flip / settlement / changes in the number of running agents will never be eaten by conflation; otherwise, events that only change node will be waited here.
// (In a real system it will still generate frames due to lastActivityAt advancement - that's an activity fact, the semantics remain unchanged).
    JSON.stringify(a.workflowActivity ?? null) === JSON.stringify(b.workflowActivity ?? null) &&
    JSON.stringify(a.pendingInteraction ?? null) === JSON.stringify(b.pendingInteraction ?? null) &&
    a.pendingInteractionSummary?.permissionCount === b.pendingInteractionSummary?.permissionCount &&
    a.pendingInteractionSummary?.userInputCount === b.pendingInteractionSummary?.userInputCount &&
    a.goalStatus === b.goalStatus &&
    a.lastActivityAt === b.lastActivityAt &&
    a.lastAssistantPreview === b.lastAssistantPreview &&
    a.createdAt === b.createdAt
  );
}

/**
* A workspace's sessions-index reduction state: Map<sessionId, SessionSummary> +
* upsert/remove delta generated (conflation key = sessionId).
 */
export class SessionsIndexProjection {
  private readonly summaries = new Map<string, SessionSummary>();

  constructor(
    readonly workspaceId: string,
    readonly logEpoch: string,
  ) {}

/** Update summary with the latest snapshot of a session; if there is a change, return upsert delta, otherwise empty. */
  upsertFromConversation(
    snapshot: ConversationSnapshot,
    extra: Omit<SessionSummaryDeriveExtra, "workspaceId">,
  ): SessionsIndexDelta[] {
    const summary = deriveSessionSummary(snapshot, {
      workspaceId: this.workspaceId,
      ...extra,
    });
    const prev = this.summaries.get(summary.sessionId);
//Downgrade defense (stored→live switching window): The live projection of cold recovery is completed in hydration
// The title before meta is empty, and the record time may be reset by resume - in the store seed
// Stable fields must not be overwritten by degraded values ​​(otherwise the sidebar will appear as "the original session disappears and new tasks pop up").
    if (prev) {
// The reading side of sessions-index should distinguish between "storage/protocol physical title" and "user explicit rename".
// Cold recovery live projection may only have the default/generated title before hydration is completed; if the old seed is already
// custom, must not be downgraded by this type of automatic title, otherwise the sidebar will flash back to the user's manual title to generate the title.
      if (prev.titleSource === "custom" && summary.titleSource !== "custom") {
        summary.title = prev.title;
        summary.titleSource = prev.titleSource;
      } else if (!summary.title && prev.title) {
        summary.title = prev.title;
// The titleSource of the old store seed can be defaulted; cold projected
// default is just a transient value. When saving the title, you must also keep the "Field Default", otherwise
// summariesEqual produces a frame of upsert with no product changes.
        if (prev.titleSource === undefined) {
          delete summary.titleSource;
        } else {
          summary.titleSource = prev.titleSource;
        }
      }
      if (prev.createdAt > 0 && summary.createdAt > prev.createdAt) {
        summary.createdAt = prev.createdAt;
      }
      if (!summary.lastAssistantPreview && prev.lastAssistantPreview) {
        summary.lastAssistantPreview = prev.lastAssistantPreview;
      }
// The cold recovery live projection phase is the initial draft before hydration is completed.
// Once the session has real content, it is impossible to return to draft; if it is used to overwrite the non-draft baseline,
// Opening a historical task will broadcast a frame of "completedSuccess→draft" pure downgrade delta,
// The UI list row status is cleared, and the corresponding workspace list is rechecked as a whole (shown as "click on the task list to reload").
// phase/sessionEnded/goalStatus has the same origin as the window and maintains the baseline.
      if (summary.phase === "draft" && prev.phase !== "draft") {
        summary.phase = prev.phase;
        summary.sessionEnded = prev.sessionEnded;
        if (summary.goalStatus === undefined && prev.goalStatus !== undefined) {
          summary.goalStatus = prev.goalStatus;
        }
      }
    }
    if (prev && summariesEqual(prev, summary)) return [];
    this.summaries.set(summary.sessionId, summary);
    return [{ op: "session.upserted", session: summary }];
  }

/** Directly put a summary (cold start/store session without live projection). */
  seed(summary: SessionSummary): void {
    this.summaries.set(summary.sessionId, summary);
  }

  /**
* Compatible with store re-reading after migration, only missing summaries are filled in; the existing live/seed state cannot be overwritten by the cold storage default state.
* Return delta for publisher to advance seq and notify list subscribers who are already online.
   */
  insertSeedIfMissing(summary: SessionSummary): SessionsIndexDelta[] {
    if (this.summaries.has(summary.sessionId)) return [];
    this.summaries.set(summary.sessionId, summary);
    return [{ op: "session.upserted", session: summary }];
  }

/** Remove a session; return remove delta if hit. */
  remove(sessionId: string): SessionsIndexDelta[] {
    if (!this.summaries.has(sessionId)) return [];
    this.summaries.delete(sessionId);
    return [{ op: "session.removed", sessionId }];
  }

  has(sessionId: string): boolean {
    return this.summaries.has(sessionId);
  }

/** Current full snapshot (sessions are unordered, sorting is client logic). */
  getSnapshot(): SessionsIndexSnapshot {
    return {
      protocolVersion: 1,
      workspaceId: this.workspaceId,
      logEpoch: this.logEpoch,
      sessions: [...this.summaries.values()],
    };
  }
}
