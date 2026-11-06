# Phase 7B Architecture Options

## Option A: Expand shared ToolContext/ToolContract
- Files: tools/mod.rs + all construction sites (28) + all ToolContract initializers.
- Pros: preserves original behavior, unifies architecture.
- Cons: large mechanical change, risk of missing sites.

## Option B: Specialized contexts
- Pros: smaller blast radius.
- Cons: duplicates logic, breaks Tool trait uniformity.

## Option C: Shared context + capability interfaces
- Pros: best long-term design.
- Cons: most complex.

## Option D: Restore earlier coherent implementation
- Not available (migration untracked, no git history).

## Recommendation
Option A (expand shared) with Option C as long-term refactor. Add fields as Option where possible to minimize construction-site churn.