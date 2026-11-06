// ============================================================
// run-launched: The host metadata of the run round → event
// ============================================================
// The engine does not read these metadata (anchor points, stage tables, parallel tables, subagent selection), and only builds run that life following the first item.
// `run-started` Transcribe verbatim once (engine.ts). The structure is separated into modules, and engine.ts and types.ts are left in
// Within the lint limit of 400 lines - they are files growing on both sides.

import type { RunEvent } from "./types.js";

/**
* The host metadata (EngineConfig.launch) that accompanies `run-launched` when the run is initiated: anchor `inputId`,
* The phase table `phaseNames` declared by the script, the `phaseAlongside` aligned with it by position, and the selection of this run subagent
* `subagentModel`, which file `scriptPath` the script comes from. None of the five engines can read it - see the field semantics in types.ts
* Comments for `run-launched`.
 */
export interface RunLaunchConfig {
  inputId: string;
  phaseNames?: string[];
  subagentModel?: string;
/** The absolute path of this run script file. */
  scriptPath?: string;
  phaseAlongside?: number[][];
}

/**
* Transcribe launch metadata to `run-launched`. Unset keys are **absent** instead of being completed with an undefined value: read side
* (Three states and projections of AmendWorkflow) Treat "not set" and "set empty" as two different things.
 */
export function runLaunchedEvent(
  launch: RunLaunchConfig,
  origin: { toolCallId?: string | undefined; parentSessionId?: string | undefined },
): RunEvent {
  const { inputId, phaseNames, subagentModel, scriptPath, phaseAlongside } = launch;
  return {
    type: "run-launched",
    inputId,
    ...(origin.toolCallId === undefined ? {} : { toolCallId: origin.toolCallId }),
    ...(origin.parentSessionId === undefined ? {} : { parentSessionId: origin.parentSessionId }),
    ...(phaseNames === undefined ? {} : { phaseNames }),
    ...(subagentModel === undefined ? {} : { subagentModel }),
    ...(scriptPath === undefined ? {} : { scriptPath }),
    ...(phaseAlongside === undefined ? {} : { phaseAlongside }),
  };
}
