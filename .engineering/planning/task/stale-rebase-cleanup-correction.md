---
format: aep.planning-md/1
id: task:stale-rebase-cleanup-correction
kind: task
status: implemented
title: Allow retirement when only a historical REBASE_HEAD remains
relations:
- implements: story:rebase-head-is-not-a-lock
revision: 4
---
## Bounded correction

The current disk cleanup reproduced 15 false lock refusals whose only worktree-local operation marker is REBASE_HEAD. An existing independently owned managed checkout contains the narrow Git-adapter correction and regression test. Reuse that reviewed patch in this integration branch without changing its original checkout or claiming its broader story complete.

Remove REBASE_HEAD from the set of operation blockers; preserve rebase-merge, rebase-apply, sequencer and all existing real lock/operation checks. A historical patch ref does not itself mean an operation is running. Validate with the existing real interrupted-rebase regression, the contributed stale-marker regression, and the full repository gate. Cleanup continues through normal finish and exact-id dry-run/apply, never by deleting Git metadata.

The broader story's doctor notes and marker-specific error presentation remain outside this correction.
