import type { PermissionOptionsPolicy, PermissionUpdate } from "@zcode/contracts";
import { OFFICIAL_CUA_PERMISSION_RULE_TOOL_NAME, type ZCodePermissionOption } from "@zcode/shared";

const PROJECT_RULE_INPUT_KEYS = ["command", "url", "file_path", "path", "pattern"] as const;

// User rejection of normal interaction permission needs to explicitly inform the model tool that it has not been executed.
// And wait for subsequent instructions from the user; the reason also serves as provider-visible tool_result.content.
export const PERMISSION_DENIED_BY_USER_CONTENT =
  "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed.";

export function buildPermissionDeniedContent(feedback?: string): string {
  const trimmed = feedback?.trim();
  return trimmed
    ? `${PERMISSION_DENIED_BY_USER_CONTENT} To tell you how to proceed, the user said:\n${trimmed}`
    : PERMISSION_DENIED_BY_USER_CONTENT;
}

/**
* Identification, internal type and display name of the session authorization option.
* The v4 wire on optionId is passed as it is (the broker relies on it to accurately hit); kind is the CLI internal value, and v4 projection maps it to
* `allowAlways` in the closed set; name is the GUI localization matching key (PermissionDialog's global name mapping table).
 */
const SESSION_ALLOW_PERMISSION_OPTION_ID = "allowSession";
export const SESSION_ALLOW_PERMISSION_OPTION_KIND = "allow_session";
const SESSION_ALLOW_PERMISSION_OPTION_NAME = "Always allow in this session";

interface PermissionOptionSource {
  input?: unknown;
  suggestedPermissionUpdates?: PermissionUpdate[];
  optionsPolicy?: PermissionOptionsPolicy;
  toolName: string;
}

/**
* Pure permission option projection common to v3 and v4. Place outside the protocol directory to avoid v4 authoritative projections from relying on old protocols.
 */
export function buildProtocolPermissionOptions(
  source: PermissionOptionSource,
): ZCodePermissionOption[] {
  const permissionUpdates = source.suggestedPermissionUpdates?.length
    ? source.suggestedPermissionUpdates
    : defaultPermissionUpdates(source);
  const officialCuaProjectScope = permissionUpdates.some((update) =>
    update.rules.some((rule) => rule.toolName === OFFICIAL_CUA_PERMISSION_RULE_TOOL_NAME),
  );
  return [
    {
      kind: "allow_once",
      name: "Allow once",
      optionId: "allow_once",
      response: {
        decision: "allow",
        reason: "Approved once",
      },
    },
// Tools can declare no-always-allow: when each call is different code, the persistence rule is not "remember this decision",
// Instead, turn off this confirmation permanently. session-always-allow is replaced by confirmation-free session scope: response
// **None** permissionUpdates -- zcodePermissionUpdateSchema on wire is strict, session semantics are determined by
// The broker is synthesized into sessionPermissionUpdates on the response side (pure memory, never project rules).
    ...(source.optionsPolicy === "no-always-allow"
      ? []
      : source.optionsPolicy === "session-always-allow"
        ? [
            {
              description: "Do not ask again for this tool in this session",
              kind: SESSION_ALLOW_PERMISSION_OPTION_KIND,
              name: SESSION_ALLOW_PERMISSION_OPTION_NAME,
              optionId: SESSION_ALLOW_PERMISSION_OPTION_ID,
              response: {
                decision: "allow" as const,
                reason: "Approved for this session",
              },
            },
          ]
        : [
            {
              description: officialCuaProjectScope
                ? "Do not ask again for official Computer Use tools in this project"
                : "Do not ask again for matching requests in this project",
              kind: "allow_always" as const,
              name: officialCuaProjectScope
                ? "Always allow Computer Use in this project"
                : "Always allow in this project",
              optionId: "allow_project",
              response: {
                decision: "allow" as const,
                permissionUpdates,
                reason: "Approved for this project",
              },
            },
          ]),
    {
      kind: "deny",
      name: "Deny",
      optionId: "deny",
      response: {
        decision: "deny",
        reason: PERMISSION_DENIED_BY_USER_CONTENT,
      },
    },
  ];
}

/**
* Session authorization is granted by the tool as a whole (without ruleContent): the script is different every time, and "this tool" is authorized rather than a certain script.
 */
export function buildSessionPermissionUpdates(toolName: string): PermissionUpdate[] {
  return [{ behavior: "allow", rules: [{ toolName }], type: "addRules" }];
}

/**
* legacy v3 (session-mapper, broker's v3 reverse RPC) does not recognize session semantics: the old desktop returns option
* response Original text, posting the session option will only get a misnamed "one-time permission". So the two strategies are on legacy
* It only shows as "crop always allow".
 */
export function toLegacyPermissionOptionsPolicy(policy: unknown): "no-always-allow" | undefined {
  switch (policy) {
    case "no-always-allow":
    case "session-always-allow":
      return "no-always-allow";
    default:
      return undefined;
  }
}

function defaultPermissionUpdates(source: PermissionOptionSource): PermissionUpdate[] {
  const ruleContent = ruleContentFromPermissionInput(source.input);
  return [
    {
      behavior: "allow",
      rules: [
        {
          toolName: source.toolName,
          ...(ruleContent ? { ruleContent } : {}),
        },
      ],
      type: "addRules",
    },
  ];
}

function ruleContentFromPermissionInput(input: unknown): string | undefined {
  if (typeof input === "string" && input.trim().length > 0) {
    return input;
  }

  const record = asRecord(input);
  for (const key of PROJECT_RULE_INPUT_KEYS) {
    const value = stringField(record, key);
    if (value) {
      return value;
    }
  }

  return undefined;
}

function asRecord(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null ? (value as Record<string, unknown>) : {};
}

function stringField(record: Record<string, unknown>, key: string): string | undefined {
  const value = record[key];
  return typeof value === "string" && value.trim().length > 0 ? value : undefined;
}
