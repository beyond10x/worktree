---
format: aep.planning-md/1
id: story:rebase-head-is-not-a-lock
kind: story
status: draft
title: A stale REBASE_HEAD is not a paused rebase
revision: 1
---
# Story: a stale `REBASE_HEAD` is not a paused rebase

## Defect
`has_operational_lock` (`crates/worktree-git/src/lib.rs:380`) treats the presence of any of
`rebase-merge`, `rebase-apply`, `sequencer`, `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`,
`REBASE_HEAD` in the worktree's git directory as a paused sequencer, and `finish` then refuses
`worktree-locked: Git marks the worktree locked`. Git keeps `REBASE_HEAD` after a rebase has
**completed** (`git rebase --continue` to the end leaves the file; only the `rebase-merge` /
`rebase-apply` directory says a rebase is in progress). Measured 2026-09-04 on two managed
worktrees (connectors, aep) after `git rebase origin/main` + `--continue`: `git worktree list
--porcelain` reported no lock, `rebase-merge` and `rebase-apply` were absent, `REBASE_HEAD` and
`ORIG_HEAD` were present, and `finish` refused both until `REBASE_HEAD` was deleted by hand. The
refusal names Git as the source of a lock Git does not hold.

## Shape
- A rebase is in progress iff `rebase-merge` or `rebase-apply` exists; a cherry-pick or revert iff
  `sequencer`, `CHERRY_PICK_HEAD` or `REVERT_HEAD`; a merge iff `MERGE_HEAD`. `REBASE_HEAD` alone is
  a stale marker and not a lock.
- The refusal names the marker it found (`rebase in progress: rebase-merge`), never "Git marks the
  worktree locked" for a state Git does not report.
- `worktree doctor` reports a stale `REBASE_HEAD` as a note, not a refusal.

## Acceptance
- A worktree whose rebase completed (no `rebase-merge`/`rebase-apply`, `REBASE_HEAD` present)
  finishes.
- A worktree with `rebase-merge` present is refused naming the directory.
- The existing refusals for `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `sequencer` are unchanged.
