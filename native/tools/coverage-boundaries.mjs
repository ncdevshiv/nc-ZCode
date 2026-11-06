// "Boundary list" of completion access control (acceptance criteria in specs/rust-backend-replacement.md §2).
//
// The reason for separate files is not abstract preferences, but `eslint(max-lines)`: the behavior of access control (evidence rules,
// Reports, self-test) and its data (which channels/members are removed by product decisions) are two types of things,
//The data is here one by one with the spec source. It is enough to only look at this table when making changes.
//
// Each reason must reference an existing governance document - `check-channel-coverage.mjs` will check the source file
// It's really in the warehouse, so "remove" cannot be used to cover up missing transplants.

// The entire channel will be removed based on product decisions (no registration, no migration). The reason must reference the spec file.
export const CHANNEL_BOUNDARY = {
  "settings-sync": "specs/zai-claim-boundary.md (upload/sync settings)",
  "usage-stats": "specs/zai-claim-boundary.md (remote usage/limit query)",
  "conversation-share": "specs/zai-claim-boundary.md (conversation sharing/remote publishing)",
  feedback: "specs/zai-claim-boundary.md (feedback report)",
  "prompt-attachment-transfer": "specs/zai-claim-boundary.md (cross-end attachment transfer)",
  "skill-sync": "specs/zai-claim-boundary.md (skill remote synchronization)",
  "mcp-sync": "specs/zai-claim-boundary.md (MCP remote synchronization)",
  "plugin-sync": "specs/zai-claim-boundary.md (plugin remote synchronization)",
  "media-preview": "specs/rust-backend-replacement.md §4.2 (overridden by file.readMediaPreview)",
  "cua-permission": "specs/rust-backend-replacement.md §4.4 (The product is not available in this host, fail-closed)",
  "cua-pip-session": "specs/rust-backend-replacement.md §4.4 (The product is not available in this host, fail-closed)",
  "oauth": "specs/zai-claim-boundary.md (fully removed - no ZCode services)",
  "coding-plan-subscription": "specs/zai-claim-boundary.md (fully removed - no ZCode services)",
  "off-peak-task": "specs/zai-claim-boundary.md (fully removed - no ZCode services)",
};

// Single member removed by bounds: glob + spec reason. Used to distinguish between "missing transplant" and "deciding not to transplant".
export const MEMBER_BOUNDARY = {
  "coding-plan-subscription": [
    ["*", "specs/zai-claim-boundary.md (purchase/team/enterprise method removed, only generic runtime config retained)"],
  ],
  "zcode-agent": [
    ["attachment*", "specs/zai-claim-boundary.md (attachment upload/handling surface removal)"],
    ["conversationAttachment*", "specs/zai-claim-boundary.md (Attachment reading/statistics are removed from the upload page)"],
    ["conversationWorkflowRun*", "specs/workflow-capability-boundary.md (workflow engine stays in TS)"],
    [
      "listSavedWorkflowRuns",
      "specs/workflow-capability-boundary.md (run records are written by the TS workflow engine)",
    ],
    ["getAppUsageStats", "specs/zai-claim-boundary.md (remote usage query removal)"],["getTaskTokenUsage", "specs/zai-claim-boundary.md (remote usage query removal)"],
    [
      "onDynamicConversationTelemetryFact",
      "specs/rust-backend-replacement.md §4.5 (telemetry trunking with R4 OTLP buckets)",
    ],
    [
      "onDynamicCuaPermissionObservation",
      "specs/rust-backend-replacement.md §4.4 (not available for CUA products)",
    ],
    // §4.5 Per-member F Evidence: There is no consumer in the Rust production build, or the event source does not exist on this host.
    [
      "onDynamicProcessResourceSample",
      "specs/rust-backend-replacement.md §4.5 (Electron resource window dedicated relay)",
    ],
    [
      "onDynamicToolExecResource",
      "specs/rust-backend-replacement.md §4.5 (Electron resource window dedicated relay)",
    ],
    [
      "onDynamicMcpResourceSamples",
      "specs/rust-backend-replacement.md §4.5 (Electron resource window dedicated relay)",
    ],
    ["onDynamicMcpTelemetry", "specs/rust-backend-replacement.md §4.5 (ARMS relay, R4 goes OTLP)"],
    [
      "collectLocalRuntimeChildProcesses",
      "specs/rust-backend-replacement.md §4.5 (in-process host has no child process tree)",
    ],
    [
      "onDynamicSessionRuntimePreferencesRequest",
      "specs/rust-backend-replacement.md §4.5 (remote connection family is not in scope)",
    ],
    [
      "respondSessionRuntimePreferences",
      "specs/rust-backend-replacement.md §4.5 (remote connection family is not in scope)",
    ],
    [
      "subscribeWorkspaceConfigV4",
      "specs/rust-backend-replacement.md §4.5 (web has no consumer, task index is responsible)",
    ],
    [
      "resyncWorkspaceConfigV4",
      "specs/rust-backend-replacement.md §4.5 (web has no consumer, task index is responsible)",
    ],
    [
      "unsubscribeWorkspaceConfigV4",
      "specs/rust-backend-replacement.md §4.5 (web has no consumer, task index is responsible)",
    ],
    [
      "onDynamicWorkspaceConfigFrame",
      "specs/rust-backend-replacement.md §4.5 (web has no consumer, task index is responsible)",
    ],
    ["prepareStorage", "specs/rust-backend-replacement.md §4.5 (TS CLI self-entry, no consumer on the front end)"],
    [
      "getStorageStartupState","specs/rust-backend-replacement.md §4.5 (TS CLI self-entry, no consumer in the front end)",
    ],
    [
      "onDynamicStorageStartupState",
      "specs/rust-backend-replacement.md §4.5 (TS CLI self-entry, no consumer in the front end)",
    ],
    [
      "listMcpServerStatuses",
      "specs/rust-backend-replacement.md §4.5 (no consumer; MCP status panel needs to determine the product first)",
    ],
    ["hasActiveCuaOperationTurn", "specs/rust-backend-replacement.md §4.5 (CUA fail-closed)"],
    [
      "testModelConnectivity",
      "specs/rust-backend-replacement.md §4.5 (alias: provider-settings already has a real path)",
    ],
    [
      "grantWorkspaceHookTrust",
      "specs/rust-backend-replacement.md §4.5 (alias: hooks channel + host admission single channel)",
    ],
    [
      "onDynamicPluginOperationProgress",
      "specs/rust-backend-replacement.md §4.5 (alias: plugin-management channel bearer)",
    ],
    ["readSessionEvents", "specs/rust-backend-replacement.md §4.4.1 (host has no event logging subsystem)"],
    [
      "onDynamicSessionEvent",
      "specs/rust-backend-replacement.md §4.5 (TS side @deprecated; session facts are carried by v4 conversation frames, this host does not have a second event source)",
    ],
    [
      "readWorkspacePresentation",
      "specs/rust-backend-replacement.md §4.4.1 (built-in slash command set not ported)",
    ],
  ],
  // zcode-session: Wiring 11, the missing items of the four F members are written in the table in §4.4.1.
  "zcode-session": [
    [
      "readWorkspacePresentation",
      "specs/rust-backend-replacement.md §4.4.1 (built-in slash command set not ported)",
    ],
    ["readSessionEvents", "specs/rust-backend-replacement.md §4.4.1 (host has no event logging subsystem)"],
    [
      "promoteDeferredDraftSession",
      "specs/rust-backend-replacement.md §4.4.1 (deferred draft registry is TS state)",
    ],
    [
      "closeDeferredDraftSession",
      "specs/rust-backend-replacement.md §4.4.1 (deferred draft registry is TS state)",
    ],
  ],
};

// Add a new channel on the Rust side that is not in the TS list (the source of the spec must be given to prevent secret implementation).
export const ADDITIONAL_CHANNELS = {  "zai-claim": "specs/rust-backend-replacement.md §4.1 / specs/zai-claim-boundary.md",
};
