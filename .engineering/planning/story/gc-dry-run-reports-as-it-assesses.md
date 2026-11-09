---
format: aep.planning-md/3
id: story:gc-dry-run-reports-as-it-assesses
kind: story
status: active
title: gc --dry-run reports each assessment as it finishes
refs:
- provider: github
  reference: beyond10x/worktree#45
relations:
- decomposes: epic:open-defects-2026-11
scope:
- confidence: cited
  path: crates/worktree-cli/src/main.rs
- confidence: inferred
  path: crates/worktree-cli/tests/gc_progress.rs
- confidence: inferred
  path: crates/worktree-git/src/lib.rs
- confidence: inferred
  path: crates/worktree-state/src/lib.rs
- confidence: cited
  path: crates/worktree/src/lib.rs
revision: 9
transitions:
- {from: "draft", to: "proposed", at: "2026-11-09T21:49:41Z", actor: "human:timo", revision: 2}
- {from: "proposed", to: "active", at: "2026-11-09T21:49:41Z", actor: "human:timo", revision: 3}
---
## Defect (GitHub #45, observed on 0.19.0)

`worktree --json gc --dry-run` on a large registry (13,318 records; 2,383 in profile scope) printed
nothing before `timeout 3600`; 78 records in one repository printed nothing in 110 s; one `--id`
returned in 6 s and three in 23 s. Assessment cost about 7 s per record and nothing is written until
the end, so a caller with a deadline gets no verdict at all.

0.19.1 to 0.19.3 bounded the Git calls of a proof and share one advertisement per assessment
(`story:recovery-proof-in-bounded-git-calls`, `story:patch-equivalence-in-bounded-git-calls`). What
no release changed: the report is still written only at the end.

## Contract

- `gc` without `--apply` (and the planning pass of `--apply`) writes one line per finished
  assessment to standard error as it finishes: the record id, whether it is eligible, and the
  refusal code when it is not. Standard output and the JSON protocol 4 envelope are unchanged
  (AGENTS.md: protocol 4 is immutable).
- With `--json` each progress line is one JSON object, so a caller killed at a deadline can read the
  assessments finished so far from standard error.
- Per-record cost is measured on a fixture registry, per phase; a cost that scales with the number
  of records in the registry instead of the records assessed is removed.

## Acceptance

- A test over a fixture repository with several finished trees (isolated state directory) shows one
  progress line per assessed record on standard error, in the order assessed, and standard output
  of the same shape as on the base commit. The test fails on the base commit.
- A test shows the lines are written before the run ends (the first line is observable while a
  later assessment has not finished, or the writer is flushed per assessment by construction and a
  unit test holds that).
- The number of Git processes for assessing N records of one repository is recorded before and
  after for N = 1 and N = 10 on the fixture; the cost of one assessment does not grow with records
  that are not assessed.
- The package gates of the crates touched pass.

## Scope

Derived 2026-11-09 by `story-scoper` at 23b9c91. Every line is **cited** (read from the tree; the story cites no path) or **inferred**.

- **Primary surface:** `crates/worktree` `Manager::gc_selected`, `crates/worktree/src/lib.rs:1295` — cited (collects every assessment into a `Vec` at 1332-1346 before anything is reported)
- **Files:** `crates/worktree-cli/src/main.rs:884` (`fn gc`, calls `gc_scoped` then a single `emit_success`) — cited
- **Symbols:** `gc_scoped`, `gc_selected`, `assess_cleanup` (`lib.rs:1739`), `CleanupAssessment`, `AssessmentsPayload` — cited
- **Also likely:** `crates/worktree-git/src/lib.rs` (`worktree_snapshot` :1153, `git worktree list` :1493, `begin_shared_observation` :1240) — inferred
- **Also likely:** `crates/worktree-state/src/lib.rs` (`list` :324, `live_lease_count`, `removal` :582) — inferred, only if the measurement blames the registry
- **Tests:** new `crates/worktree-cli/tests/gc_progress.rs` — inferred (fixture pattern from `crates/worktree-cli/tests/gc_scope.rs:131`); unit test in the `lib.rs` test module (fake `GitPort` near :3432) — inferred
- **Documents:** `CHANGELOG.md`, `README.md` — written by the coordinator at the close, not by the unit
- **Confidence:** medium — the reporting half is read from the tree; the cost half depends on a measurement nobody has made
- **Would collide with:** any unit touching `gc_selected`/`assess_cleanup` in `crates/worktree/src/lib.rs`, `fn gc` in `worktree-cli/src/main.rs`, or the snapshot code in `worktree-git/src/lib.rs`
- **Not established:** which phase carries a registry-scaled cost; no Git-process counter exists in the tests; whether a progress callback changes the public `gc`/`gc_scoped` signatures.
