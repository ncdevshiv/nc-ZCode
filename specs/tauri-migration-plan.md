# Tauri Migration Plan: Remove Electron, Use Tauri + Rust Backend Exclusively

**Date:** 2026-11-05 (updated 2026-11-06)  
**Author:** SD-002 (System Designer)  
**Status:** In progress (Tauri live; Rust backend ~55–65% code-only)
<!-- trace:2026-11-06 docs sync — replaces stale Planning/6% claims. dev:desktop→dev:tauri live (package.json:15,21); embedded backend real (nc-code-shell/src/backend.rs:66 build + :93 serve). % is code-only estimate. -->
**Goal:** Remove Electron entirely and use only Tauri with Rust backend exclusively. Keep plugins/skills intact. Keep model/provider agnostic.

## Executive Summary

This plan outlines the migration from the current dual/hybrid setup (Electron + Tauri) to a pure Tauri + Rust backend architecture. Currently:
- `dev:desktop` points to `dev:tauri` at root level (`package.json:15` `dev:desktop` → `dev:tauri`; `package.json:21` builds `packages/web` then `cargo run -p nc-code-shell`) — **live on disk 2026-11-06**
- Tauri shell (`native/crates/nc-code-shell/`, 32 `.rs` files) embeds Rust backend: `src/backend.rs:66` `nc-code-app::build_with_ports` assembles the real channel registry, `:93` `nc_code_server::serve` serves it on `127.0.0.1:0`; frontend is `packages/web` build
- Electron codebase still exists in `packages/desktop/` with full main/host/preload/renderer infrastructure
- Two different frontend builds: Electron uses `packages/desktop/src/renderer` (MessagePort transport), Tauri uses `packages/web` (WebSocket transport)
- Backend: Electron spawns Node host processes running TS services; Tauri uses embedded Rust backend (**~55–65% code-only**, 54 `nc-code-*` crates glob-counted 2026-11-06; the ~6% figure was the 2026-09-23 snapshot, see `RUST-PORT-MATRIX.md` 2026-11-06 update)

## Current Architecture Comparison

### Electron Setup (`packages/desktop/`)
**Process Model:** Multi-process (Main, Renderer, Host processes)
- **Main Process** (`src/main/`): 150+ files managing windows, app lifecycle, IPC, host process spawning, native integrations, auto-updater, etc. Heavy use of Electron APIs.
- **Host Processes** (`src/host/`): Separate Node processes spawned by main via `utilityProcess.fork()`. Run `@zcode/services/node` implementations. Communicate with renderer via MessagePort.
- **Preload** (`src/preload/`): Context bridge exposing APIs to renderer via `window.electronAPI` / similar.
- **Renderer** (`src/renderer/`): Vite-built React app, connects via `connectViaMessagePort()` from `@zcode/client`. Has its own platform adapter (`desktopPlatform.ts`).
- **Transport**: MessagePort between renderer and host processes, IPC between renderer and main, host-to-main communication.
- **Build**: tsup for main/host, Vite for renderer. Uses `electron-builder` for packaging.

### Tauri Setup (`native/crates/nc-code-shell/`)
**Process Model:** Single Rust process
- **Tauri Shell** (`src/main.rs`): Rust binary managing windows, tray, native APIs via Tauri commands. Implements `IPlatformService`-shaped commands.
- **Embedded Backend** (`src/backend.rs`): Starts `nc-code-app` which builds all services, starts `nc-code-server` on `127.0.0.1:0`, serves `packages/web/dist`, navigates window to server.
- **Frontend**: Uses `packages/web` build (built separately). Connects via `connectViaWebSocket()` to embedded server.
- **Transport**: HTTP/WebSocket to embedded Rust server. Native capabilities via Tauri `invoke()` commands called from web frontend's `createTauriPlatform()`.
- **Build**: Cargo build for Rust, Vite build for web frontend.

## Key Differences

| Aspect | Electron | Tauri |
|---|---|---|
| Language | TypeScript/Node.js + Electron | Rust + Tauri |
| Process Model | Multi-process (main, renderer, host) | Single process with embedded server |
| Backend Services | TS services in host processes (`packages/services/node`) | Rust services in `native/crates/nc-code-services-*` via `nc-code-app` |
| Frontend | `packages/desktop/src/renderer` (MessagePort) | `packages/web` (WebSocket) |
| Native APIs | Electron APIs + custom IPC | Tauri commands + plugins |
| Host Process Management | Complex spawning/lifecycle in `desktopHostProcess.ts` | None - embedded in Rust |
| Window Management | Electron BrowserWindow | Tauri WebviewWindow |
| Packaging | electron-builder | Tauri bundler (currently disabled in config) |

## Migration Goals

1. **Remove all Electron dependencies and code**
   - Remove `electron`, `electron-builder`, `@electron/*` packages
   - Delete `packages/desktop/src/main/`, `packages/desktop/src/host/`, `packages/desktop/src/preload/`
   - Remove Electron-specific configs (`electron-builder.config.js`, etc.)
   - Remove tsup config for main/host builds

2. **Use Tauri as the only desktop shell**
   - All desktop functionality goes through `native/crates/nc-code-shell/`
   - Use Tauri commands for all native capabilities
   - Tray, menus, windows managed by Tauri

3. **Use Rust backend exclusively**
   - All business logic must run in Rust services (`nc-code-app` + `nc-code-services-*`)
   - No spawning of Node host processes
   - No TS services running in separate processes
   - WebSocket transport to embedded Rust server only

4. **Consolidate frontend**
   - Use `packages/web` as the sole frontend (already used by Tauri)
   - Remove or repurpose `packages/desktop/src/renderer` (MessagePort-based)
   - Ensure `packages/web` works fully in Tauri mode with all features
   - The web frontend already has Tauri support via `createTauriPlatform()` in `main.tsx`

5. **Keep plugins/skills intact**
   - All plugin/skill infrastructure must remain functional
   - Plugin loading/discovery must work with Rust backend

6. **Keep model/provider agnostic**
   - Provider system must remain flexible
   - No hardcoding of specific providers

## Detailed Migration Plan

### Phase 0: Preparation & Analysis (Investigation - Current)
- [x] Understand Electron setup structure
- [x] Understand Tauri setup structure  
- [x] Understand communication patterns
- [x] Map differences
- [ ] Audit all Electron usages across codebase
- [ ] Audit Rust backend coverage vs TS services
- [ ] Identify gaps that need to be filled in Rust

### Phase 1: Rust Backend Completion
**Priority:** Critical - Rust backend must be feature-complete

The RUST-PORT-MATRIX shows only ~6% ported **at the 2026-09-23 snapshot**; the 2026-11-06 refresh counts ~55–65% code-only (54 crates, `native/Cargo.toml:7`). We still need to ensure the Rust backend handles everything the TS host/services do before Electron removal.

**Tasks:**
1. **Audit service coverage**
   - Map all channels/services in TS (`packages/services`, `packages/server`) to Rust equivalents
   - Identify missing services in `native/crates/nc-code-services-*`
   - Check `nc-code-app` composition - does it register all needed channels?
   
2. **Complete critical services**
   - Agent/task/session management
   - Workspace management
   - Provider/model management
   - Terminal/PTY
   - File system operations
   - Git operations
   - Extensions/plugins
   - Authentication/config
   - Remote connections (if needed)
   - All channels the frontend expects

3. **Ensure channel compatibility**
   - Rust server must expose all channels the web frontend expects
   - Protocol compatibility with `@zcode/rpc` and `@zcode/shared` channels
   - Test WebSocket communication end-to-end

4. **Host process replacement**
   - Everything that Electron host did must be done by Rust backend
   - Resource management, telemetry, etc. may need Rust implementations

**Files to investigate:**
- `native/crates/nc-code-app/src/lib.rs` - service composition
- `native/crates/nc-code-services-*` - all service implementations
- `packages/services/src/` - TS services to understand what needs porting
- `packages/shared/src/channels.ts` - channel definitions

### Phase 2: Frontend Consolidation
**Goal:** Migrate fully to `packages/web` frontend

**Tasks:**
1. **Ensure packages/web has all features**
   - Compare `packages/web/src/main.tsx` with `packages/desktop/src/renderer/src/main.tsx`
   - The desktop renderer has some desktop-specific features (MessagePort connection, different platform setup)
   - Web has Tauri support via `createTauriPlatform()` - need to ensure parity
   - Check for features only in desktop renderer that need to be ported to web

2. **Update web frontend for Tauri-only mode**
   - Ensure Tauri platform adapter is complete and robust
   - Remove or update any Electron-specific assumptions
   - Test all native capabilities through Tauri commands

3. **Handle differences**
   - Desktop renderer connects via MessagePort to host processes
   - Web connects via WebSocket to server
   - Service initialization differs
   - Some features may behave differently

4. **Build process**
   - Tauri shell already builds and serves `packages/web/dist`
   - Ensure `packages/web` build is production-ready for Tauri

**Files to investigate:**
- `packages/web/src/main.tsx` vs `packages/desktop/src/renderer/src/main.tsx`
- `packages/web/vite.config.ts`
- `packages/client/src/messageport.ts` vs `packages/client/src/websocket.ts`

### Phase 3: Tauri Shell Enhancement
**Goal:** Ensure nc-code-shell can handle all desktop functionality

**Tasks:**
1. **Audit Tauri commands coverage**
   - Compare all Electron IPC handlers in `packages/desktop/src/main/desktopMainIpc*.ts` with Tauri commands in `nc-code-shell/src/main.rs`
   - Identify missing native capabilities
   - Implement missing commands in Rust

2. **Platform adapter parity**
   - Ensure `createTauriPlatform()` in `packages/web/src/main.tsx` implements all `IPlatformService` methods
   - Some methods may be missing or have different implementations
   - Check for TODOs or stub implementations

3. **Native integrations**
   - Tray menu - already implemented
   - Deep links - implemented
   - File dialogs - implemented
   - Notifications - implemented
   - Browser view/embedded browser - implemented in Rust
   - Window management - implemented
   - Auto-updater - Electron has it, Tauri may need different approach
   - Crash reporting/telemetry - need to ensure compatibility

4. **Embedded browser**
   - Electron has complex embedded browser implementation
   - Tauri shell has `browser_view.rs` and related files - need to ensure feature parity
   - CDP integration exists

**Files to investigate:**
- `native/crates/nc-code-shell/src/main.rs` - all Tauri commands
- `packages/desktop/src/main/desktopMainIpc*.ts` - all IPC handlers
- `packages/shared/src/platform.ts` - IPlatformService interface
- `packages/web/src/main.tsx` - Tauri platform adapter

### Phase 4: Remove Electron Code
**Goal:** Delete all Electron-specific code

**Tasks:**
1. **Remove from packages/desktop**
   - Delete `src/main/` directory (entire Electron main process)
   - Delete `src/host/` directory (host processes)
   - Delete `src/preload/` directory
   - Delete `src/renderer/` if fully replaced by web frontend (or keep minimal if needed)
   - Delete Electron configs: `electron-builder.config.js`
   - Update `package.json` - remove electron deps and scripts
   - Update tsup configs, vite configs as needed
   - Remove `native/` subdirectory in desktop if Electron-specific

2. **Remove from root**
   - Update `package.json` scripts - already points to tauri, but clean up
   - Remove electron-related deps
   - Update build scripts

3. **Remove Electron dependencies**
   - From `packages/desktop/package.json`: electron, electron-builder, @electron/*, etc.
   - From root if present
   - Check for any other electron usages

4. **Clean up references**
   - Search for `electron` imports/usages
   - Update imports, configs
   - Remove Electron-specific code paths

**Caution:** Need to be careful not to break things. Do incremental removal.

### Phase 5: Update Build System & Scripts
**Tasks:**
1. **Root package.json**
   - `dev:desktop` already points to `dev:tauri` - good
   - `build:desktop-agent` and related may need updates
   - Clean up Electron-specific scripts
   - Update `typecheck` script if it references desktop tsconfigs differently

2. **packages/desktop/package.json**
   - Simplify to Tauri-related scripts only or remove if not needed
   - Or repurpose for Tauri build helpers

3. **Tauri build configuration**
   - Update `native/crates/nc-code-shell/tauri.conf.json`
   - Configure bundling properly
   - Set up icons, etc.
   - The bundle is currently disabled (`"active": false`)

4. **Build pipeline**
   - Ensure `bun run build:tauri` works
   - Ensure `bun run dev:tauri` works
   - Test packaging

### Phase 6: Testing & Validation
**Tasks:**
1. **Functionality testing**
   - All core features work
   - Native dialogs, file operations
   - Terminal/PTY
   - Embedded browser
   - Window management
   - Deep links
   - etc.

2. **Run verification commands**
   - `bun run typecheck`
   - `bun run lint`
   - `bun run architecture:check`
   - `bun run native:test` (Rust tests)
   - `bun run verify:pre-push`

3. **E2E testing**
   - Test with Rust backend exclusively
   - Ensure no Electron code paths remain

4. **Update specs if behavior changes**
   - Review if any specs need updating per ZCode rules
   - Update architecture docs if needed

## Risk Assessment

### High Risk
1. **Rust backend incompleteness** - RUST-PORT-MATRIX dated 2026-09-23 shows only 6% ported (snapshot); 2026-11-06 refresh is ~55–65% code-only. Residual host wiring / long-tail parity remains the biggest risk, not "nothing ported".
2. **Feature parity** - Electron has years of features, Tauri/Rust may be missing some
3. **Host process functionality** - Complex logic in host processes may not be ported
4. **Embedded browser differences** - Different implementations between Electron and Tauri

### Medium Risk
1. **Frontend differences** - MessagePort vs WebSocket have different characteristics
2. **Native API differences** - Some Electron APIs don't map cleanly to Tauri
3. **Build system complexity** - Many interconnected build scripts
4. **Plugin system** - Must remain intact

### Low Risk
1. **Tray/menu** - Already implemented in Tauri
2. **File dialogs** - Already implemented
3. **Basic window management** - Already implemented

## Mitigation Strategies

1. **Incremental migration** - Don't delete everything at once. Test at each step.
2. **Keep Electron code until Rust backend is verified** - Can temporarily keep both
3. **Feature flags** - Use flags to switch between implementations if needed
4. **Thorough testing** - Test each feature after migration
5. **Follow existing patterns** - Look at how Tauri is already implemented for guidance
6. **Consult specs** - Follow rust-native-crates.md, rust-pure-native-roadmap.md

## Files to Delete/Modify

### Delete (Phase 4)
```
packages/desktop/src/main/          # Entire Electron main process
packages/desktop/src/host/          # Entire host process code
packages/desktop/src/preload/      # Preload scripts
packages/desktop/electron-builder.config.js
packages/desktop/tsup.config.ts (if only for main/host)
```

### Major Modifications
```
packages/desktop/package.json      # Remove electron deps/scripts
packages/desktop/vite.config.ts    # May need simplification
root package.json                  # Clean up
packages/web/src/main.tsx          # Ensure Tauri completeness
native/crates/nc-code-shell/*     # Fill gaps
native/crates/nc-code-app/*       # Ensure full service composition
```

### Possibly Remove/Repurpose
```
packages/desktop/src/renderer/     # If fully replaced by web
packages/desktop/src/scheduler/   # Check if Electron-specific
```

## Questions to Resolve

1. **Is the Rust backend complete enough?** The RUST-PORT-MATRIX says 0 fully replaceable modules. But the task says to do this migration - maybe the Rust backend needs to be completed as part of this migration?
2. **Which frontend to use?** Tauri already uses `packages/web`. The task says "use only Tauri" - probably standardize on web frontend.
3. **What about the desktop package?** Does it become obsolete or get repurposed for Tauri helpers?
4. **Auto-updater?** Electron has it, Tauri has tauri-plugin-updater but may not be configured.
5. **Remote connections?** Web says "Remote connect is not supported in Web mode yet" and Tauri says similar - need to check if this is acceptable.

## Next Steps

1. **Complete Phase 0** - Audit all Electron usages and Rust backend coverage
2. **Update board** with detailed findings
3. **Identify critical gaps** in Rust backend
4. **Plan Rust backend completion** if needed
5. **Start incremental changes** only after full plan is reviewed

## Notes

- Keep plugins/skills intact - verify no changes needed there
- Follow ZCode rules - update specs if behavior changes
- All changes must pass lint, typecheck, architecture checks
- The Rust backend must be used exclusively - no Node services in production
- Model/provider agnostic - must not break flexibility
