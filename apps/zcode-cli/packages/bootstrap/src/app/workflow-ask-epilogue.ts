// ============================================================
// Quality endnotes for dwf ask
// ============================================================
//
//Independent file: workflow-driver.ts has already exceeded max-lines, and this section is pure copy + a schema detection,
// Has nothing to do with the driver's state machine.
//
// The endnote was branched by persona's tool level - the zero-tool GLM subagent read "every finding
// cites what you read or ran" and then find a tool it doesn't have and issue `escalate("placeholder")`.
// The entire tool stall exits, each sub-agent
// All have a complete working toolset, and endnotes return to a text.

/**
*Quality endnotes for each ask: content criteria for results—evidence,
* The difference between "running through" and "believing", telling the truth if you can't do it, and escalating when blocked. Add typed / untyped; typed
* when schema endnotes follow. When the top-level properties of the schema contain `evidence` / `confidence`, name these two
* If there is no field, it will not be mentioned.
* Format (this note is the contract): two blank lines + separator line + title line + bullet list.
 */
export function qualityEpilogue(schema: unknown): string {
  const fields = topLevelSchemaProperties(schema);
  return [
    "",
    "",
    "---",
    "Standard for this result:",
    "- Every finding cites what you read or ran: path and line for code; the exact command and its output for a check; the part of the ask for material the ask itself gave you.",
    "- A check counts as passed only if you ran it during this ask. Otherwise report it as not run.",
// The subagent picks the command that turns green the fastest when "the test is passed" - ask refers to the entire suite, which runs a file;
// ask is talking about e2e, which runs single test. As long as the contract is honest, this sentence does not care about its standards: the substitute will report according to the substitute, and tell him which one he ran away from.
    "- Run the check the ask names, at the scale it names. A narrower or faster substitute — one test file for the suite, a build for the tests — is reported as what it is, never as the ask's check; say the exact command you ran.",
    "- Anything you could not do, verify, or find is stated as such — never filled with a plausible guess.",
    ...(fields.has("evidence") ? ["- Put each finding's citation in its `evidence` field."] : []),
    ...(fields.has("confidence")
      ? ["- Rate `confidence` honestly; a low value with a reason beats a confident guess."]
      : []),
    "- If you are blocked by something outside your reach, call `escalate` instead of inventing a value.",
  ].join("\n");
}

/** The set of keys for the schema's top-level `properties`; the empty set for non-object schemas or when absent. */
function topLevelSchemaProperties(schema: unknown): Set<string> {
  if (typeof schema !== "object" || schema === null) return new Set();
  const properties = (schema as { properties?: unknown }).properties;
  if (typeof properties !== "object" || properties === null) return new Set();
  return new Set(Object.keys(properties as Record<string, unknown>));
}
