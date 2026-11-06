// ============================================================
//Runtime guard for submit profile
// ============================================================
// Independent file: workflow-driver.ts has already exceeded max-lines, and this read-only driver's deps / sink / session state,
// Independent of the rest of the state machine.
//
// Static profile is a derivation of the site map may-set; if a typed ask falls on a session that does not match it, it may only be due to insufficient analysis.
// Precise - it's not the script's fault, nor should run fail silently:
// - `mono` but with a different schema: change this session's submit_result back to a generic declaration and let the runtime recalculate the tool surface,
// From now on, the session will be run as generic (the endnote will contain the entire schema). The cost is that the cache prefix is ​​invalidated once, and the correctness is not affected.
// - `untyped`: This runtime does not have a submit port at all (the factory profile does not inject it), and there is no tool to replace it——
// Use DriverError to make **this ask** fail and tell which site it is, instead of letting it exhaust nudge and then continue
// ResultNotSubmitted failed, disguising the analysis problem as a model problem.

import { submitResultToolEntry } from "@zcode/core";
import {
  GENERIC_SUBMIT_PROFILE,
  canonicalJson,
  refToString,
  WorkflowError,
  type AskMessage,
  type InstanceRef,
  type WorkflowReportSink,
} from "@zcode/dynamic-workflow";
import type { AgentRuntimeWorkflowDriverDeps, SessionState } from "./workflow-driver-types.js";

/** Log event name when static profile does not match ask. */
const SUBMIT_PROFILE_MISMATCH_EVENT = "dynamic_workflow.submit_profile.mismatch";

/**
* Make the session's submit_result form match this typed ask. Returning false indicates that ask has failed and the caller must not dispatch any more
*turn. A generic session with the same schema as a mono session is passed unchanged.
 */
export function ensureSubmitProfileFits(
  deps: AgentRuntimeWorkflowDriverDeps,
  sink: WorkflowReportSink,
  state: SessionState,
  instance: InstanceRef,
  message: AskMessage,
): boolean {
  const profile = state.submitProfile;
  if (profile.kind === "generic") return true;
  if (profile.kind === "mono" && canonicalJson(profile.schema) === canonicalJson(message.schema)) {
    return true;
  }
  if (profile.kind === "untyped") {
    sink.askFailed(
      instance,
      new WorkflowError(
        "DriverError",
        `Typed ask ${refToString(instance)} reached subagent session ${state.sessionId} whose static ` +
          `submit profile is "untyped" (no submit_result registered): the ask→actor analysis missed this ask.`,
      ),
    );
    return false;
  }
  deps.logger?.warn?.(
    "Dynamic workflow submit profile mismatch: falling back to the generic submit_result",
    {
      event: SUBMIT_PROFILE_MISMATCH_EVENT,
      module: "bootstrap.app",
      instance: refToString(instance),
      runId: deps.runId ?? "run",
      sessionId: state.sessionId,
    },
  );
// Same registry, same name: register overrides the typed entry; silently, because overwriting is exactly the intention. The cache must be invalidated,
// Otherwise getTools will continue to send the old typed declaration to the model.
  state.runtime.getToolRegistry().register(submitResultToolEntry, { silentDuplicateWarning: true });
  state.runtime.invalidateToolCache();
  state.submitProfile = GENERIC_SUBMIT_PROFILE;
  return true;
}
