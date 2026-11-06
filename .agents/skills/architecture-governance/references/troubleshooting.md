# Troubleshooting

- `module-dependency`: use a public contract or declare the dependency in both the local manifest and policy after confirming its owner.
- `deep-import`: use the target module's declared public entrypoint.
- `cycle`: move shared types into a contract or invert the dependency through a port.
- `missing-module-artifact`: add the managed module's manifest, contract, example or contract document reported by the check.
- `expired-exception`: resolve the underlying violation and remove the expired exception; do not silently extend it.
- Changed-file scope: `bun run architecture:check --changed` selects differences from `HEAD` plus untracked files. Run `bun run architecture:check` for a full scan; committed changes are no longer a dirty-worktree diff.
- Baseline mismatch: inspect the failures first. Use `bun run architecture:baseline:update` only for an explicitly reviewed baseline change.

The check currently resolves relative imports. Inspect workspace aliases and package imports separately; a passing result does not prove that every dependency was analyzed.
