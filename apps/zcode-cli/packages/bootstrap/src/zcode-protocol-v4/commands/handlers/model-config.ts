// Model configuration command group: switchModelConfig (transported from the old server-operations.switchModelConfig semantics)
// + switchCollaborationMode(additive, UI mode selector)
// + applyRequestedSessionConfig (createSession.config consumer commonware).
// One file per command group: handler pure function (host, envelope) → CommandResult|undefined,
// Decision logic directly drives core (app.setModel / app.setMode / runtime.emit*), without going through the old protocol op.
import type { CollaborationMode, ModelSelection } from "@zcode/contracts";
import type {
  CommandEnvelope,
  CommandPayloadMap,
  CommandResult,
} from "@zcode/shared/zcode-protocol-v4";
import { V4CommandNoopError } from "../../v4-gateway.js";
import { runSessionModelConfigMutation } from "../../model-config-mutation.js";
import { requireRecord } from "../record-access.js";
import type { V4CommandCoreHost, V4SessionRecordView } from "../types.js";

/** Noop reasonCode for switching with the same value. */
const CONFIG_UNCHANGED = "config.unchanged";

/**
* The target Provider is not in the current Environment Registry. Gateway maps it as failed ACK;
* The caller needs to refresh the Provider Config of the current Environment instead of re-pushing the Host Snapshot to the Worker.
 */
class V4ProviderNotInRegistryError extends Error {
  readonly reasonCode = "provider.notInRegistry";
  constructor(providerId: string) {
    super(`provider "${providerId}" is not in the workspace model registry`);
  }
}

/**
* Before cutting the model, confirm that the target Provider already exists in the current Environment Registry. `applied:false` converts to
* Structured field error (gateway → failed ACK). Host maintains the old Entry behavior when no capabilities are injected.
 */
async function ensureProviderClientReady(
  host: V4CommandCoreHost,
  sessionId: string,
  providerId: string,
): Promise<void> {
  if (!host.ensureProviderAvailable) return;
  const outcome = await host.ensureProviderAvailable(sessionId, providerId);
  if (!outcome.available && outcome.reason === "provider_not_in_registry") {
    throw new V4ProviderNotInRegistryError(providerId);
  }
//session_not_found and other reasons: requireRecord has been verified on the handler side first, and the theory is not up to date;
// Don't throw away the details (let the subsequent setModel take the existing path/report an error) to avoid swallowing the real positioning.
}

function createModelSelection(provider: string, model: string, thought: string | undefined) {
  return {
    providerId: provider,
    modelId: model,
    ...(thought ? { options: { reasoningLevel: thought } } : {}),
  };
}

function readActualThought(
  record: V4SessionRecordView,
  fallbackSelection?: ModelSelection,
): string {
  const thought = record.app.getThoughtLevel();
  if (thought !== undefined) return thought;
  return fallbackSelection?.options?.reasoningLevel ?? "";
}

/** switchCollaborationMode command value range (command.ts z.enum has the same origin; auto is not included for non-users). */
const SWITCHABLE_MODES: ReadonlySet<string> = new Set(["build", "edit", "plan", "yolo"]);

/**
* switchModelConfig: switch session model selection. When crossing models, replace app.setModel with provider client + model.
* When using the same model, thought is the explicit thinking depth switch, and then the ModelSelected - v4 projected config area is reissued.
*Updating and switching modelChange marker all rely on this event (reducer onModelSelected).
 *
* Description of behavioral equivalence: The old protocol path does not have an active turn guard (switching is also allowed during operation), and is not added here to keep it consistent.
 */
async function switchModelConfig(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const payload = envelope.payload as CommandPayloadMap["switchModelConfig"];
  const record = requireRecord(host, envelope.sessionId);
  return runSessionModelConfigMutation(record.app, async () => {
// previous must be snapshotted within the serialization critical section and before setModel. registry fallback may be ranked before this command
// Previously, if you read before queuing, you will get the expired previous, and split the noop/event order from the runtime truth value.
    const previousSelection = record.app.runtime.getSessionModelSelection();
    const previousModelSelection =
      previousSelection &&
      createModelSelection(
        previousSelection.providerId,
        previousSelection.modelId,
        previousSelection.options?.reasoningLevel,
      );
    const previousThought = readActualThought(record, previousSelection);
    const modelIdentityChanged =
      previousSelection?.providerId !== payload.provider ||
      previousSelection?.modelId !== payload.model;
    const requestedThought = payload.thought.trim();
    const thoughtChanged =
      Boolean(requestedThought) && requestedThought !== previousSelection?.options?.reasoningLevel;
// Same value switch closure: hit runtime current value → noop ACK (config.unchanged),
// Cannot be swallowed silently with accepted - after the seeds are aligned, "UI display value = runtime true value" is established.
// The client uses this to distinguish between "effective" and "originally this value".
    if (!modelIdentityChanged && !thoughtChanged) {
      throw new V4CommandNoopError(CONFIG_UNCHANGED);
    }
// Before setModel, the current Environment Registry confirms that the target Provider is available.
    await ensureProviderClientReady(host, record.app.sessionId, payload.provider);
    let actualThought = previousThought;
    let nextModelSelection: ModelSelection;
    if (modelIdentityChanged) {
      const result = await record.app.setModel(`${payload.provider}/${payload.model}`);
      actualThought = result.thoughtLevel ?? readActualThought(record);
      if (requestedThought && record.app.listThoughtLevels().includes(requestedThought)) {
// This is a pin even if the user explicitly selects the same gear as the default. The setter must be called to let
// Session Selection saves this explicit leaf and cannot swallow the intent because the effective value is the same.
        const thoughtResult = await record.app.setThoughtLevel(requestedThought);
        actualThought = thoughtResult.thoughtLevel;
      }
      nextModelSelection = createModelSelection(
        payload.provider,
        payload.model,
        requestedThought && record.app.listThoughtLevels().includes(requestedThought)
          ? requestedThought
          : undefined,
      );
    } else {
// Provider/model is the same only if the user explicitly changes it. Thought; illegal values ​​fail before any model changes.
      const result = await record.app.setThoughtLevel(requestedThought);
      actualThought = result.thoughtLevel;
      nextModelSelection = createModelSelection(payload.provider, payload.model, actualThought);
    }
    await record.app.runtime.emitModelSelected({
      modelSelection: nextModelSelection,
      ...(actualThought ? { effectiveReasoningLevel: actualThought } : {}),
      previousModelSelection,
      supportedThoughtLevels: record.app.listThoughtLevels(),
// The trace link structure is transparently transmitted from the record (session root trace), and is not associated with the traceId if it is not started at the command layer.
      traceContext: record.traceContext,
    });
    return undefined;
  });
}

/**
* switchCollaborationMode: switch agent collaboration mode (plan/build/edit/yolo).
* app.setMode uniformly updates independent execution status, persists and publishes SessionModeChanged,
* The command layer will no longer reissue the second event.
* v4 projection reducer onSessionModeChanged updates config.mode accordingly.
* Same value switch → noop ACK: silent return undefined will be regarded as accepted, if the projection seed is missing
* It will be superimposed into a user-visible problem of "no response when clicking full access" - CLI thinks it is yolo
* Returned early, but the projection still stopped at the seed build, and the client could not identify it. Therefore an explicit ACK is required.
 */
async function switchCollaborationMode(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const payload = envelope.payload as CommandPayloadMap["switchCollaborationMode"];
  const record = requireRecord(host, envelope.sessionId);
  const mode = payload.mode as CollaborationMode;
  const previousMode = record.app.getMode();
  if (previousMode === mode && !record.app.runtime.getPlanEnabled()) {
    throw new V4CommandNoopError(CONFIG_UNCHANGED);
  }
  await record.app.setMode(mode);
  return undefined;
}

/**
* createSession.config consumption sharedware ("createSession.config must be consumed"):
* Merge with "request config to override runtime default", which will only take effect on the parts that are different from the current runtime value.
* And reissue events that have the same source as the switch command (ModelSelected / SessionModeChanged) - the log is self-sufficient,
* The projection has an existing reducer end and does not rely on the second writing path.
 *
* Why use the event instead of changing the seed directly: the publisher has been created during the createSessionRecord event wiring.
* The seed reads the runtime default before the application requests config; event reissue not only corrects the projection, but also allows
* The fact "what model is used for the first launch" is entered into the log (cold recovery replay can be restored). When the first selection prev is empty
* reducer does not produce modelChange marker (onModelSelected "The first selection does not count as switching"), no noise.
 *
* Partial failure semantics: The session has been created successfully, and the failure of config application should not cause createSession (record leak
* Change to a failed ACK (not worth it) - after the caller catches it, it is downgraded to warn, and the session keeps the runtime default.
 */
export async function applyRequestedSessionConfig(
  host: V4CommandCoreHost,
  record: V4SessionRecordView,
  config: NonNullable<CommandPayloadMap["createSession"]["config"]>,
): Promise<void> {
  await runSessionModelConfigMutation(record.app, async () => {
    const previousSelection = record.app.runtime.getSessionModelSelection();
    const previousModelSelection =
      previousSelection &&
      createModelSelection(
        previousSelection.providerId,
        previousSelection.modelId,
        previousSelection.options?.reasoningLevel,
      );
    const previousThought = readActualThought(record, previousSelection);
    const requestedSelection = config.modelSelection;
    const targetProvider =
      requestedSelection?.providerId ?? config.provider?.trim() ?? previousSelection?.providerId;
    const targetModel =
      requestedSelection?.modelId ?? config.model?.trim() ?? previousSelection?.modelId;
    const targetThought =
      requestedSelection?.options?.reasoningLevel ?? config.thought?.trim() ?? "";
    const modelIdentityChanged =
      targetProvider !== previousSelection?.providerId ||
      targetModel !== previousSelection?.modelId;
    const thoughtChanged =
      Boolean(targetThought) &&
      targetThought !==
        (requestedSelection ? previousSelection?.options?.reasoningLevel : previousThought);
    if (targetProvider && targetModel && (modelIdentityChanged || thoughtChanged)) {
// When the first Provider is not in the Registry, provider.notInRegistry is thrown, and the capture at createSession is downgraded to
// warn (the session keeps the runtime default and is not created continuously), the semantics are consistent with the existing config application failure.
      await ensureProviderClientReady(host, record.app.sessionId, targetProvider);
      let actualThought = previousThought;
      if (modelIdentityChanged) {
        const result = await record.app.setModel(`${targetProvider}/${targetModel}`);
        actualThought = result.thoughtLevel ?? readActualThought(record);
      }
      if (targetThought && record.app.listThoughtLevels().includes(targetThought)) {
        const result = await record.app.setThoughtLevel(targetThought);
        actualThought = result.thoughtLevel;
      } else if (requestedSelection?.options?.reasoningLevel) {
// Explicit options for formally structured Selection must fail-closed; only old flat config
// Retain the published compatibility behavior of "use default if not supported by target".
        await record.app.setThoughtLevel(targetThought);
      }
      if (modelIdentityChanged || actualThought !== previousThought) {
// The draft warm-up config may carry the thought of the previous model. Reserved if the target model does not support it
// The compatible gear that has been parsed by setModel still publishes the target model event to avoid creating a runtime/projection split session.
        await record.app.runtime.emitModelSelected({
          modelSelection: createModelSelection(
            targetProvider,
            targetModel,
            targetThought && record.app.listThoughtLevels().includes(targetThought)
              ? targetThought
              : undefined,
          ),
          ...(actualThought ? { effectiveReasoningLevel: actualThought } : {}),
          previousModelSelection,
          supportedThoughtLevels: record.app.listThoughtLevels(),
          traceContext: record.traceContext,
        });
      }
    }
  });

// mode: payload.config.mode is a wide string (schema default compatible), and the value range ends here.
  const mode = config.mode;
  if ((mode && SWITCHABLE_MODES.has(mode)) || config.planEnabled !== undefined) {
    await record.app.runtime.setExecutionState(
      {
        ...(mode && SWITCHABLE_MODES.has(mode) ? { mode } : {}),
        ...(config.planEnabled !== undefined ? { planEnabled: config.planEnabled } : {}),
      },
      record.traceContext,
    );
  }

// followupMode: The runtime default is queue (the initial projection value is the same), only the non-default value needs to be written explicitly——
// runtime.setFollowupMode has no identical value guard (unconditional append event), and explicitly passing "queue" will cause idle delta.
  if (config.followupMode && config.followupMode !== "queue") {
    await record.app.setFollowupMode(config.followupMode);
  }
}

export const modelConfigHandlers = { switchModelConfig, switchCollaborationMode };
