# Recovery Plan

1. Preserve snapshot (done: G:\\audit-snapshot).
2. Do not reset or clean.
3. Manually review each corrupted file against snapshot and HEAD.
4. Fix syntax errors one file at a time.
5. Run cargo check per crate.
6. Run targeted tests.
7. Only after green builds, resume F1 validation.

No broad search-and-replace. No file restoration from HEAD without review.