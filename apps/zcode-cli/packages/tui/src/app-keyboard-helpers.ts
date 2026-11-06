import type { KeyEvent } from "@mbears/opentui-core";
import { clampIndex } from "./app-input.js";
import { modelOptionValue } from "./app-model-ref.js";
import type {
  EffortCommandSelectionState,
  ModeCommandSelectionState,
  ModelCommandSelectionState,
  SlashCommand,
  SlashSelectionState,
} from "./app-model.js";
import type { TuiEffortOption, TuiModeOption, TuiModelOption } from "./types.js";

export function completeSlashCommand(
  commands: readonly SlashCommand[],
  slashSelection: SlashSelectionState | undefined,
  setDraftValue: (value: string) => void,
): boolean {
  if (!slashSelection || commands.length === 0) return false;
  const command = commands[clampIndex(slashSelection.selectedIndex, commands.length)];
  if (!command) return false;
  setDraftValue(`/${command.name} `);
  return true;
}

export function completeModelCommand(
  models: readonly TuiModelOption[],
  modelSelection: ModelCommandSelectionState | undefined,
  setDraftValue: (value: string) => void,
): boolean {
  if (!modelSelection || models.length === 0) return false;
  const model = models[clampIndex(modelSelection.selectedIndex, models.length)];
  if (!model) return false;
  setDraftValue(`/model ${modelOptionValue(model)}`);
  return true;
}

export function completeEffortCommand(
  efforts: readonly TuiEffortOption[],
  effortSelection: EffortCommandSelectionState | undefined,
  setDraftValue: (value: string) => void,
): boolean {
  if (!effortSelection || efforts.length === 0) return false;
  const effort = efforts[clampIndex(effortSelection.selectedIndex, efforts.length)];
  if (!effort) return false;
  setDraftValue(`/effort ${effort.id}`);
  return true;
}

export function completeModeCommand(
  modes: readonly TuiModeOption[],
  modeSelection: ModeCommandSelectionState | undefined,
  setDraftValue: (value: string) => void,
): boolean {
  if (!modeSelection || modes.length === 0) return false;
  const mode = modes[clampIndex(modeSelection.selectedIndex, modes.length)];
  if (!mode) return false;
  setDraftValue(`/mode ${mode.id}`);
  return true;
}

export function shouldHandleInputHistoryNavigation({
  draftValue,
  inputHistoryActive,
}: {
  draftValue: string;
  inputHistoryActive: boolean;
}): boolean {
  // model streaming output still leaves the composer editable; blocking
  // on busy made history recall unavailable exactly when users queue followups.
  return inputHistoryActive || draftValue.length === 0;
}

export function isModeSwitchKey(key: KeyEvent): boolean {
  return key.name === "tab" && key.shift;
}

/**
* `+` / `-` Expand/collapse the **complete** judgment of all workflow cards (key position + two-layer gate), and export it as a pure function for direct testing.
* Key position: bare `+` / `-` without modifier keys; Ctrl/Meta combinations have other attributes and will not be recognized.
* Gate: The draft must be empty and have at least one workflow card - otherwise these two keys must be entered into the draft as usual
* (The first character when pasting a diff is +/-, eating it will destroy the input).
 */
export function workflowExpansionActionFor({
  key,
  draftValue,
  hasCards,
}: {
  key: KeyEvent;
  draftValue: string;
  hasCards: boolean;
}): "expand" | "collapse" | undefined {
  if (key.ctrl || key.meta) return undefined;
  if (draftValue.length > 0 || !hasCards) return undefined;
  const char = key.name ?? key.raw;
  if (char === "+") return "expand";
  if (char === "-") return "collapse";
  return undefined;
}

export const PROMPT_DRAFT_CLEARED_STATUS = "Ready.";
export const CTRL_C_EXIT_PROMPT = "Press Ctrl-C again to exit.";
export const CTRL_C_EXIT_CONFIRMATION_WINDOW_MS = 2_000;

export function shouldClearPromptDraftOnCtrlC(draftValue: string): boolean {
  return draftValue.length > 0;
}

export type CtrlCExitGuard = {
  lastPressAtMs: number | undefined;
};

type CtrlCExitIntent = "confirm_exit" | "show_prompt";

export function createCtrlCExitGuard(): CtrlCExitGuard {
  return { lastPressAtMs: undefined };
}

export function resolveCtrlCExitIntent(
  guard: CtrlCExitGuard,
  nowMs: number,
  windowMs = CTRL_C_EXIT_CONFIRMATION_WINDOW_MS,
): CtrlCExitIntent {
  const lastPressAtMs = guard.lastPressAtMs;
  const elapsedMs = lastPressAtMs === undefined ? undefined : nowMs - lastPressAtMs;
  if (elapsedMs !== undefined && elapsedMs >= 0 && elapsedMs <= windowMs) {
    guard.lastPressAtMs = undefined;
    return "confirm_exit";
  }

  guard.lastPressAtMs = nowMs;
  return "show_prompt";
}

export function resetCtrlCExitGuard(guard: CtrlCExitGuard): void {
  guard.lastPressAtMs = undefined;
}
