// TUI controller of dwf running image (same hook convention as useSidebarController / useInputHistory).
//
// Only state and derivation: mirror body, card connection table, expanded collection. Wiring for session events remains in app.tsx
// (Subscription needs to get applySessionEvent, which in turn needs the setter of this hook, which will form a loop when put together).
import React from "react";
import type { TuiCopy } from "@zcode/i18n";
import type { WorkflowRunProgressEnvelope } from "@zcode/shared/zcode-protocol-v4";
import type { Message } from "./app-model.js";
import type { TuiOptions } from "./types.js";
import { useWorkflowRunSeeding } from "./app-workflow-seed.js";
import {
  EMPTY_TUI_WORKFLOW_MIRROR,
  applyWorkflowProgressToMirror,
  buildTuiWorkflowCardIndex,
  type TuiWorkflowCard,
  type TuiWorkflowMirror,
  seedWorkflowMirror,
  type TuiWorkflowRunSeed,
} from "./app-workflow-mirror.js";

/** Expansion control surface for `+`/`-`. Hanging on an object allows app.tsx to pass only one more prop. */
export type TuiWorkflowExpansionControls = {
/** Capturing `+`/`-` is only allowed if there is at least one card - otherwise those two keys must be entered into the draft as usual. */
  hasCards: boolean;
  expandAll: () => void;
  collapseAll: () => void;
};

type TuiWorkflowRunsController = {
  mirror: TuiWorkflowMirror;
  setMirror: React.Dispatch<React.SetStateAction<TuiWorkflowMirror>>;
  cardsByToolCallId: ReadonlyMap<string, TuiWorkflowCard>;
  expandedRunIds: ReadonlySet<string>;
  toggleExpansion: (runId: string) => void;
  seed: (seeds: readonly TuiWorkflowRunSeed[]) => void;
  expansion: TuiWorkflowExpansionControls;
};

export function useTuiWorkflowRuns(input: {
  copy: TuiCopy;
  options: TuiOptions;
  setMessages: React.Dispatch<React.SetStateAction<Message[]>>;
}): TuiWorkflowRunsController {
  const [mirror, setMirror] = React.useState<TuiWorkflowMirror>(EMPTY_TUI_WORKFLOW_MIRROR);
  const [expandedRunIds, setExpandedRunIds] = React.useState<ReadonlySet<string>>(
    () => new Set<string>(),
  );

  const cardsByToolCallId = React.useMemo(() => buildTuiWorkflowCardIndex(mirror), [mirror]);

  const toggleExpansion = React.useCallback((runId: string) => {
    setExpandedRunIds((current) => {
      const next = new Set(current);
      if (next.has(runId)) next.delete(runId);
      else next.add(runId);
      return next;
    });
  }, []);

// Cold reseeding only moves the display name (label / updatedAt). Play the following playback in running state: journal’s **real and ordered** events
// Reduced by the same reducer - the "never feed summary synthesis events to the reducer" prohibition is for out-of-order synthesis,
// Not true for engine events replayed by sequence.
  const seed = React.useCallback((seeds: readonly TuiWorkflowRunSeed[]) => {
    setMirror((current) => seedWorkflowMirror(current, seeds));
  }, []);
  const replay = React.useCallback((envelopes: readonly WorkflowRunProgressEnvelope[]) => {
    setMirror((current) => envelopes.reduce(applyWorkflowProgressToMirror, current));
  }, []);
  const mirrorRef = React.useRef(mirror);
  mirrorRef.current = mirror;
  const knownRunIds = React.useCallback(
    () => new Set(mirrorRef.current.state.runs.map((run) => run.runId)),
    [],
  );

// Expand/collapse all: TUI deliberately does not have a card selection mechanism (no panel, no cursor), so `+`/`-` can only work on the whole.
  const runIdsKey = [...cardsByToolCallId.values()].map((card) => card.runId).join("\u0000");
  const expandAll = React.useCallback(() => {
    setExpandedRunIds(new Set(runIdsKey.length === 0 ? [] : runIdsKey.split("\u0000")));
  }, [runIdsKey]);
  const collapseAll = React.useCallback(() => setExpandedRunIds(new Set<string>()), []);

// Reseed once when mounting and hit interrupted notice; no polling (the second clock is the negative teaching material of the legacy panel).
  useWorkflowRunSeeding({
    copy: input.copy,
    ...(input.options.listWorkflowRuns === undefined
      ? {}
      : { listWorkflowRuns: input.options.listWorkflowRuns }),
    ...(input.options.replayWorkflowRuns === undefined
      ? {}
      : { replayWorkflowRuns: input.options.replayWorkflowRuns }),
    knownRunIds,
    replay,
    seed,
    setMessages: input.setMessages,
  });

  return {
    mirror,
    setMirror,
    cardsByToolCallId,
    expandedRunIds,
    toggleExpansion,
    seed,
    expansion: { hasCards: cardsByToolCallId.size > 0, expandAll, collapseAll },
  };
}
