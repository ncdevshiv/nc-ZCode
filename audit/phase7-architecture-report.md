# Phase 7 Architecture Report

ToolContext/ToolContract mismatch analysis:

- `automation_tools.rs` expects fields: automation, session_id, model_ref, tool_call_id, subagent, ask_user, plan_mode, plan, background, sessions, workflow.
- `ToolContract` expects: max_model_bytes, preview_direction.
- Current `tools/mod.rs` definitions lack all of these.
- Other non-compiled modules (node_repl, workflow, authoring) also use these fields.
- Aligning requires updating 28+ ToolContext construction sites and all ToolContract initializers.

Resolution chosen: NONE (substantial architectural change required).

No production functionality removed. Build remains blocked.