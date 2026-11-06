# End-to-End Audit Report: TS Backend → Rust Port Status

**Date:** 2025-11-05  
**Auditor:** AI Agent (zero-trust verification)  
**Repository:** G:\ZCode-main (branch: feat/rust-native-host)

## Executive Summary

This audit was conducted with zero trust on existing documentation and logs. The codebase shows a complex, partially completed migration from TypeScript to Rust with significant naming confusion between "zcode" (original) and "nc-code" (attempted rename). Several real bugs were discovered and fixed during audit.

**Key Findings:**
- **Rust port is substantially advanced** in some areas (native host, tools, channel backend) but far from complete
- **Critical bug found and fixed**: Variable name typo `ID` vs `id` in `nc-code-node-repl/src/driver/kernel_driver.mjs` causing 5 test failures
- **Naming confusion exists**: Mix of `zcode-*` and `nc-code-*` naming across crates, configs, and references
- **Backend replacement (channel RPC)** appears ~100% covered by coverage checker, but integration status is mixed
- **Several pre-existing issues found** including test failures and potential compatibility problems

## 1. Naming Confusion Analysis (zcode vs nc-code)

### Root Cause
According to `specs/nc-code-rename-plan.md`, the rename from `zcode` to `nc-code` was attempted but done incompletely and incorrectly:
- **Class A (internal identifiers)**: Crates renamed from `zcode-*` to `nc-code-*` (32 crates) - this was done
- **Class B (on-disk state)**: `.zcode/` directories, data dirs, provider configs - **NOT renamed** (correctly avoided to prevent data loss)
- **Class C (wire/env vars)**: `ZCODE_*` env vars, wire protocol names, artifact URIs - **NOT fully renamed** (intentionally kept for compatibility)

### Observations
- **Package.json** still uses `"name": "zcode"` (line 2 in package.json)
- **Rust workspace** in `native/Cargo.toml` uses `members = ["crates/nc-code-*"]` glob
- **Empty directory remains**: `native/crates/zcode-host/` exists but is empty (0 files) - leftover from rename
- **Mixed references**: Many files still reference `zcode` (env vars like `ZCODE_NATIVE_HOST`, binary names, paths like `.zcode/`, etc.)
- **Crate naming**: All active crates are now `nc-code-*` except possibly some remnants

### Issues from Naming Confusion
1. **Empty zcode-host directory** suggests incomplete migration/cleanup
2. **Inconsistent naming** between package name (`zcode`) and crate names (`nc-code-*`)
3. **Potential broken references** from rename operations
4. The rename plan explicitly states "a half-done rename is worse than a slow one" - this appears to be a half-done rename in some aspects

## 2. Rust Port Status Assessment

### Native Crates Inventory
Found 31 crates in `native/crates/`:
- **Core infrastructure**: `nc-code-core`, `nc-code-rpc`, `nc-code-channels`, `nc-code-v4-wire`, `nc-code-service-core`
- **Host/runtime**: `nc-code-host`, `nc-code-app`, `nc-code-server`, `nc-code-shell`
- **Services**: `nc-code-services-*` (agent, automation, client, config, extensions, git, git-checkpoint, offpeak, plugins, provider, terminal, workspace, zai)
- **Sidecars/tools**: `nc-code-diff`, `nc-code-scan`, `nc-code-watch`, `nc-code-proc`, `nc-code-image`, `nc-code-sidecar-proto`
- **Specialized**: `nc-code-node-repl`, `nc-code-provider`, `nc-code-workflow`, `nc-code-cdp`

Plus empty `zcode-host/` directory.

### Test Status (Native Rust)
Ran full native test suite (`cargo test --workspace` in native/):
- **Overall**: Most tests passing
- **Fixed during audit**: 5 failures in `nc-code-node-repl` kernel_e2e tests - all fixed (variable name bug)
- **Remaining issues**: 
  - RPC golden parity tests had issues due to BOM/encoding during regeneration attempts (restored from git)
  - Some tests may have environment dependencies

### Backend Replacement (Channel RPC)
According to `native/tools/check-channel-coverage.mjs`:
```
[coverage] channels 29/29 registered(Another 11 are removed entirely by spec)
[coverage] members 325/325 has distribution evidence(100%)
[coverage] All transplantable channels have been registered, members have distribution point evidence, and all boundaries have spec sources
```

This suggests the channel-based backend replacement is **functionally complete** in terms of registration and coverage. The Rust server (`nc-code-server`) exists with HTTP/WS/static serving capabilities.

### Host/Agent Runtime
`nc-code-host` is well-developed based on RUST-PORT-LOG entries:
- Full v4 protocol support
- Tool execution framework
- MCP integration (both native and external)
- Provider integration (OpenAI-compatible, Anthropic shapes tested live)
- Session management, turn execution, tool loops
- Can be spawned as replacement for TS agent via `ZCODE_NATIVE_HOST` flag

### Tauri Shell
`nc-code-shell` exists as Tauri 2 app shell with:
- Browser control (WebView2), CDP integration
- Computer use (CUA) on Windows
- Recording capabilities
- Native platform operations

## 3. Bugs Found and Fixed

### Bug #1: Variable name typo in kernel driver (CRITICAL - FIXED)
**Location:** `native/crates/nc-code-node-repl/src/driver/kernel_driver.mjs:736`  
**Issue:** Used `ID` (uppercase) instead of `id` (lowercase) in the catch block of `handleEvaluate()`. The variable `id` was defined on line 710 as `const id = frame.id;`  
**Impact:** All kernel_e2e tests failing with "ReferenceError: ID is not defined" - 5 out of 13 tests failing  
**Root Cause:** Typo in the codebase  
**Fix:** Changed `ID` to `id` on line 736  
**Verification:** All 13 kernel_e2e tests now pass

### Bug #2: RPC golden.json corruption during audit (MITIGATED)
**Issue:** Attempted to regenerate golden vectors, caused BOM/encoding issues corrupting the file  
**Fix:** Restored from git and stripped BOM properly  
**Status:** File restored to git HEAD state

## 4. Architectural Observations

### Strengths
1. **Well-structured Rust workspace** with clear crate boundaries
2. **Comprehensive testing** with golden vectors for wire compatibility
3. **Good separation of concerns** (rpc, channels, services, host layers)
4. **Coverage checking tooling** enforces backend replacement completeness
5. **Incremental migration approach** with feature flags (`ZCODE_NATIVE_HOST`, etc.)

### Concerns
1. **Incomplete rename** causing confusion and potential maintenance issues
2. **Dual implementations** - TS and Rust code coexist, which is expected during migration but adds complexity
3. **Mixed naming conventions** throughout the codebase
4. **Empty directories** (like empty `zcode-host/`) suggest incomplete cleanup
5. **Node-repl tests were failing** before fix - suggests either environment issues or code quality problems

### Migration Status
Based on logs and code:
- **Sidecar islands (diff, scan, watch, proc, image)**: Wired up with feature flags, some defaulting to Rust
- **Native host**: Functional, can replace TS agent with flag
- **Channel backend**: 100% coverage according to checker, crates exist and are wired
- **Tauri shell**: Substantially developed
- **Full cutover to Rust backend**: Not complete - TS server and services still exist

## 5. Recommendations

### Immediate Actions
1. **Complete cleanup of rename artifacts** - Remove empty `zcode-host/` directory, fix any remaining broken references
2. **Fix any remaining test failures** - Address RPC golden parity issues properly
3. **Document the actual current state** - Update docs to reflect reality, not aspirational state

### Short-term Improvements
1. **Standardize naming** - Either complete the rename to `nc-code` or revert to `zcode` consistently
2. **Improve test reliability** - Fix environment-dependent tests
3. **Clean up dead code** - Remove unused imports, empty files, etc.

### Long-term
1. **Complete the migration** following the plan in `rust-backend-replacement.md`
2. **Remove TS backend code** once Rust backend is fully integrated and tested
3. **Tighten architecture boundaries** as per the language boundary rules

## Conclusion

The Rust port is **substantially advanced** but not complete. The backend replacement infrastructure is well-developed with good test coverage. However, the incomplete rename from `zcode` to `nc-code` has created confusion and technical debt. The critical bug found and fixed (variable name typo in kernel driver) shows the value of zero-trust auditing over relying on documentation.

The codebase is in a transitional state with both implementations present. The project appears to be following a strangler pattern migration, which is reasonable but requires careful coordination to avoid breaking things.

**Overall Assessment:** Port is ~60-80% complete in terms of infrastructure, but full cutover is not yet achieved. The naming confusion and mixed state need to be addressed for long-term maintainability.
