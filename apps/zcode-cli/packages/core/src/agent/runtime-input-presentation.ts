import { parseRuntimeInputPresentation } from "@zcode/contracts";
import type { RuntimeMessageMetadata } from "./message-history.js";

/** The new tag determines the real input identity, and the coordinator cannot be allowed to inherit the real_user of the guide. */
export function runtimeInputMetadata(value: unknown): RuntimeMessageMetadata | undefined {
  const inputPresentation = parseRuntimeInputPresentation(value);
  if (!inputPresentation) return undefined;
  return {
    source: inputPresentation === "user_steer" ? "real_user" : "legacy_synthetic",
    inputPresentation,
  };
}
