// Cold start/image playback, reseeding and interrupted notice when resuming the session.
//
//Three disciplines:
// 1. The running state **only** comes from the playback: `replayWorkflowRuns` is handed over to the journal that is cast in sequence order and live
// Same kind of progress envelope, fed to the shared reducer one by one - ordered real event replay will not make the phase fall back (the ban is for
// "Use abstracts to synthesize out-of-order events", the prohibition still holds: abstracts only supplement display names).
// 2. Reseeding only moves the display name: label / updatedAt; no status, no guessing resumable (that is the status bit, moved by the reducer).
// 3. No second clock: only checked once when mounting and app replacement, no polling, no setInterval.
import React from "react";
import type { TuiCopy } from "@zcode/i18n";
import type { WorkflowRunProgressEnvelope } from "@zcode/shared/zcode-protocol-v4";
import type { Message } from "./app-model.js";
import type { TuiListWorkflowRuns, TuiReplayWorkflowRuns, TuiWorkflowRunSummary } from "./types.js";
import type { TuiWorkflowRunSeed } from "./app-workflow-mirror.js";

/** Summary → Reseeding entry. Only the display facts provided by the server will be moved, and the default will remain the default (no label). */
function workflowRunSeedFromSummary(summary: TuiWorkflowRunSummary): TuiWorkflowRunSeed {
  return {
    runId: summary.runId,
    ...(summary.label === undefined ? {} : { label: summary.label }),
    ...(summary.updatedAt === undefined ? {} : { updatedAt: summary.updatedAt }),
  };
}

/** A notice text. label returns runId by default - one less label in the list is a degeneration, not an error. */
function workflowInterruptedNoticeText(seed: TuiWorkflowRunSeed, copy: TuiCopy): string {
  return copy.transcript.workflow.interruptedNotice({
    label: seed.label ?? seed.runId,
    runId: seed.runId,
  });
}

/**
* Add the resumable run prompt line to the transliteration. Zero output without run (spec's Excludes).
 *
* Use system lines instead of fake user lines: this is not what the user said.
 */
function appendWorkflowInterruptedNotices(
  messages: Message[],
  seeds: readonly TuiWorkflowRunSeed[],
  copy: TuiCopy,
): Message[] {
  if (seeds.length === 0) return messages;
  return [
    ...messages,
    ...seeds.map((seed) => ({
      content: workflowInterruptedNoticeText(seed, copy),
      role: "system" as const,
    })),
  ];
}

/**
* Run prompts after starting / `/resume`: those that the server says `resumable`.
 *
* **Deliberately not sorted**: `updatedAt` is a purely display field, and the port annotation explicitly prohibits readers from reordering it - sorting is the responsibility of the storage layer
* (most recently updated first), if the read side is queued again, it will drift with the tie-break on the server side.
 */
function interruptedWorkflowNotices(
  summaries: readonly TuiWorkflowRunSummary[],
): readonly TuiWorkflowRunSeed[] {
  return summaries.filter((summary) => summary.resumable === true).map(workflowRunSeedFromSummary);
}

/**
* When mounting, play back first and then reseed.
 *
* The order is important: replay comes first, and the real number of steps will be given as soon as the card appears; after reseeding, only the name will be given. Even if the query fails twice, it should not be allowed.
* TUI won't get up - Live events will still draw the run on the fly.
 *
* The callback is called indirectly via ref, and only the identities of the two query functions are placed in the dependency: the input parameter object is new every time it is rendered, and it is a direct dependency.
* will cause this effect to rerun the query every time it is rendered (that is, the second clock).
 */
export function useWorkflowRunSeeding(input: {
  copy: TuiCopy;
  listWorkflowRuns?: TuiListWorkflowRuns;
  replayWorkflowRuns?: TuiReplayWorkflowRuns;
/** Existing runs in the image (this process has received events) - playback excludes them. */
  knownRunIds: () => ReadonlySet<string>;
  replay: (envelopes: readonly WorkflowRunProgressEnvelope[]) => void;
  seed: (seeds: readonly TuiWorkflowRunSeed[]) => void;
  setMessages: React.Dispatch<React.SetStateAction<Message[]>>;
}): void {
  const latest = React.useRef(input);
  latest.current = input;
  const { listWorkflowRuns, replayWorkflowRuns } = input;

  React.useEffect(() => {
    if (!listWorkflowRuns && !replayWorkflowRuns) return;
    let cancelled = false;
    void (async () => {
      if (replayWorkflowRuns) {
        try {
          const envelopes = await replayWorkflowRuns({
            excludeRunIds: latest.current.knownRunIds(),
          });
          if (cancelled) return;
          if (envelopes.length > 0) latest.current.replay(envelopes);
        } catch {
// Playback failure should not prevent TUI from getting up.
        }
      }
      if (!listWorkflowRuns) return;
      let summaries: readonly TuiWorkflowRunSummary[];
      try {
        summaries = await listWorkflowRuns();
      } catch {
        return;
      }
      if (cancelled || summaries.length === 0) return;
      latest.current.seed(summaries.map(workflowRunSeedFromSummary));
// The order is server-side order (most recently updated first) - the port annotation prohibits read side-ordering.
      const resumable = interruptedWorkflowNotices(summaries);
      if (resumable.length === 0) return;
      latest.current.setMessages((current) =>
        appendWorkflowInterruptedNotices(current, resumable, latest.current.copy),
      );
    })();
    return () => {
      cancelled = true;
    };
  }, [listWorkflowRuns, replayWorkflowRuns]);
}
