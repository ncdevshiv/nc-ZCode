# Audit Methodology

Audit date: 2026-11-01
Repository: G:\ZCode-main
Commit: 84c0f241fdcb02af9f5f3f74261807ed64d194f1
Branch: feat/rust-native-host
Working tree: 1,115 tracked modified, 4 deleted, 6 renamed, 199 untracked.

## Preservation
No source files were modified, reset, cleaned, stashed, or overwritten.
A temporary Git worktree was created at G:\tmp-audit-worktree and removed after inspection.

## Evidence sources
- Direct source inspection.
- Executed commands (see test/build evidence).
- Git status and diff.

## Exclusions
- Test files and generated files are excluded from production capability counts where identifiable.
- Due to symlinked node_modules under packages/desktop, file counts are approximate.

## Limitations
- Full per-symbol enumeration of ~21,323 TS exports was not completed.
- Semantic verification of all 325 service members was not completed.
- Ledger is at capability-group level with explicit unverified entries.