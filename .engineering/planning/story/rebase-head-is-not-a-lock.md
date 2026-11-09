---
format: aep.planning-md/3
id: story:rebase-head-is-not-a-lock
kind: story
status: active
title: A stale REBASE_HEAD is not a paused rebase
relations:
- decomposes: epic:open-defects-2026-11
scope:
- confidence: cited
  path: crates/worktree-cli/src/main.rs
- confidence: inferred
  path: crates/worktree-cli/tests/doctor.rs
- confidence: inferred
  path: crates/worktree-domain/src/lib.rs
- confidence: cited
  path: crates/worktree-git/src/lib.rs
- confidence: cited
  path: crates/worktree/src/lib.rs
revision: 9
transitions:
- {from: "draft", to: "proposed", at: "2026-11-09T21:49:41Z", actor: "human:timo", revision: 2}
- {from: "proposed", to: "active", at: "2026-11-09T21:49:41Z", actor: "human:timo", revision: 3}
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

## Scope

Derived 2026-11-09 by `story-scoper` at 23b9c91, remaining work only (the `REBASE_HEAD` removal shipped through `task:stale-rebase-cleanup-correction`). Every line is **cited** or **inferred**.

- **Primary surface:** `crates/worktree-git/src/lib.rs` — cited; `has_operational_lock` (:1021) returns a bare `bool`, folded into `locked` at :1192, so the marker name is lost there
- **Files:** `crates/worktree/src/lib.rs:3166` — cited; `require_unlocked` emits "Git marks the worktree locked" (same text at :2094 for migration)
- **Files:** `crates/worktree-domain/src/lib.rs:214` — inferred; `WorktreeSnapshot` has only `locked: bool` and needs a field carrying the marker
- **Files:** `crates/worktree-cli/src/main.rs:1186` — cited; `doctor` and `DoctorReport` (:1178) inspect no worktrees and have no notes field today
- **Tests:** `crates/worktree-git/src/lib.rs` (`a_stale_rebase_head_leaves_the_worktree_unlocked`, :2166) and `crates/worktree-cli/tests/doctor.rs` — inferred
- **Documents:** `CHANGELOG.md` — written by the coordinator at the close
- **Confidence:** medium — both refusal sites and doctor were read; how the marker travels and which worktrees doctor scans are open design choices
- **Would collide with:** any unit touching `WorktreeSnapshot`, `worktree_snapshot` in `worktree-git`, the refusal helpers in `crates/worktree/src/lib.rs`, or `doctor` in `worktree-cli` — cited
- **Not established:** whether `WorktreeSnapshot` is persisted (it derives `Deserialize`); a new field touches about 25 struct literals in `crates/worktree/src/lib.rs` tests; the wording for a real Git lock, `BISECT_*` and `*.lock` files, which the story does not list.
