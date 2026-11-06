# Phase 7B — Historical Tool Architecture Reconstruction

## Findings

### Two incompatible generations

1. **Minimal generation** (compiled): `tools/mod.rs` declares 9 modules (bash, fs_tools, list_models, read_image, search_tools, skill, todo, webfetch, automation_tools). ToolContext has 12 fields. ToolContract has 9 fields.

2. **Rich generation** (uncompiled): node_repl, workflow, authoring, runs, background_tools, plan, read_session_context, send_message, web_search, mcp_tools. These reference ToolContext fields: session_id, model_ref, tool_call_id, subagent, ask_user, plan_mode, plan, background, sessions, workflow, automation. And ToolContract fields: max_model_bytes, preview_direction.

### Origin
- Git history is unavailable (migration is untracked). HEAD does not contain these files.
- The richer generation was written but never integrated into the shared definitions.
- The minimal generation is the currently compiled baseline.

### Conclusion
The divergence is between a minimal compiled subset and a richer uncompiled subset. The shared ToolContext/ToolContract is incomplete relative to the richer tools. The safest minimal correction is to extend the shared definitions with the missing fields (as Option where appropriate) and update all construction sites.