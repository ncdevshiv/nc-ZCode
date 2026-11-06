import type { ZCodeApp } from "../app/types.js";

const modelConfigMutationTails = new WeakMap<ZCodeApp, Promise<void>>();

/**
* The model configuration of the same session App has only one serialization critical section.
 *
* Provider registry fallback and user `switchModelConfig` used to be called directly respectively
* `setModel`, two asynchronous chains may be interleaved into "the last starting directory overwrites the user selection", and the sequence of events will also be the same as
* The final runtime value is inconsistent. Here, all model/thought changes are serialized according to the App identity; if it fails, it will only end this time.
* operation, does not pollute subsequent tail. WeakMap does not extend the session life cycle.
 */
export async function runSessionModelConfigMutation<T>(
  app: ZCodeApp,
  operation: () => Promise<T>,
): Promise<T> {
  const previousTail = modelConfigMutationTails.get(app) ?? Promise.resolve();
  const currentOperation = previousTail.catch(() => undefined).then(operation);
  const currentTail = currentOperation.then(
    () => undefined,
    () => undefined,
  );
  modelConfigMutationTails.set(app, currentTail);
  try {
    return await currentOperation;
  } finally {
    if (modelConfigMutationTails.get(app) === currentTail) {
      modelConfigMutationTails.delete(app);
    }
  }
}
