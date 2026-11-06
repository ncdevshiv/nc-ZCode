# Audit Notes

> **CRITICAL RULE**: Never trust docs, logs, or comments without fully auditing the actual codebase. Always verify against implementation.
> 
> Last audit: 2026-11-05

## Key Findings from Recent Audit

### Workflow Engine
- **Reality**: Workflow engine core is ported to Rust (`native/crates/nc-code-workflow/`) and integrated into `native/crates/nc-code-host/src/workflow_service/`. 
- **Stale docs**: `specs/workflow-capability-boundary.md` previously stated engine stays in TypeScript - updated to reflect Rust implementation.
- **Note**: TS compiler/sandbox may still be used in some paths by design; TS workflow stack may remain live path until native host is default.

### ZCode Services Removal
- **Completed**: All ZCode services (auth, OAuth, quota, coding plan subscription, usage stats, account providers) removed.
- **Specs updated**: `specs/zai-claim-boundary.md`, `specs/disabled-features.md`, `specs/rust-backend-replacement.md` updated.
- **Coverage**: Channel coverage passes with removed channels marked in boundaries.

### Rust Port Status
- Channel coverage: 100% (26/26 channels registered, 301/301 members with evidence)
- Many services ported to Rust
- TS backend still required for: desktop integration/e2e, workflow live path in some cases, delete gate not met

## Audit Checklist
- [ ] Verify against actual files, not just docs
- [ ] Check both specs and implementation
- [ ] Look for conflicting information across docs
- [ ] Test/verify with tools (coverage, etc.)
- [ ] Update stale docs when found
- [ ] Document discrepancies
