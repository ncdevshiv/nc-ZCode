# Phase 7B Implementation Blueprint

## Recommended architecture
Extend ToolContext and ToolContract with missing fields (Option where appropriate).

## Files requiring changes
- native/crates/zcode-host/src/tools/mod.rs (struct definitions)
- All 28 ToolContext construction sites
- All ToolContract initializers in compiled tools
- Non-compiled modules that use these fields (already written)

## Required tests
- Tool registration tests.
- Automation tool invocation (success + failure).
- Input validation.
- Permission enforcement.
- Error propagation.
- Tool result serialization.
- Existing tool regression tests.

## Migration order
1. Add fields to ToolContext/ToolContract.
2. Update construction sites (turn.rs, mcp_tools.rs, test ctx helpers).
3. Update ToolContract initializers in compiled tools.
4. Verify automation_tools.rs compiles.
5. Compile non-declared modules (node_repl, workflow) to catch further mismatches.
6. Run full workspace check.

## Rollback strategy
- Revert ToolContext/ToolContract changes; automation_tools remains excluded.

## Known risks
- 28 construction sites must all be updated.
- Non-compiled modules may have additional mismatches.
- Tests in automation_tools.rs use richer context.

## Acceptance criteria
- zcode-host compiles.
- Workspace all-target compiles.
- All existing tests pass.
- Queue F1 regression tests pass.