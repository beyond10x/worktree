---
format: aep.planning-md/3
id: story:strip-dry-run-is-fast
kind: story
status: active
title: A profile-wide archive dry run finishes in a foreground call
relations:
- decomposes: epic:open-defects-2026-11
scope:
- confidence: cited
  path: crates/worktree-cli/src/main.rs
- confidence: inferred
  path: crates/worktree-cli/tests/strip_dry_run_speed.rs
- confidence: inferred
  path: crates/worktree-domain/src/build_output.rs
- confidence: cited
  path: crates/worktree-git/src/archive.rs
- confidence: inferred
  path: crates/worktree-git/src/lib.rs
- confidence: cited
  path: crates/worktree/src/lib.rs
revision: 10
transitions:
- {from: "draft", to: "proposed", at: "2026-11-09T21:49:41Z", actor: "human:timo", revision: 2}
- {from: "proposed", to: "active", at: "2026-11-09T21:49:41Z", actor: "human:timo", revision: 3}
---
## Outcome

`worktree prune-archives --strip-build-output --scope profile --dry-run` over a machine's whole
archive root finishes fast enough to run in the foreground of an agent's tool call, and says how
long it took.

## Why

On 2026-10-10 the 0.15.0 release binary took 2016 s for that dry run over 1398 archives,
34.71 GiB (`~/.cache/worktree-ctl/strip-profile-0.15.0-20261010.json`). Over this repository's
own 6 archives (0.93 GB), a debug build took 41 s for the strip dry run and 4 s for the plain
prune dry run. Two debug-build runs over the profile hit a 3000 s timeout and printed nothing,
because the report is written only at the end.

## To find out first

Where the time goes, measured per phase on one machine-sized archive root: reading and hashing
each `dirty.patch` (the digest check and the section scan each read the whole file), `git ls-tree
-r` of HEAD per archive, the remote fetch per repository, and the bundle fallback. Probably the
patch is read more than once and the tracked-path listing is repeated per archive of the same
repository; that is a hypothesis until measured.

## Candidate changes, decided after measuring

- one pass over each patch for digest and section scan;
- the tracked-path listing and remote proof cached per repository and HEAD within one run;
- archives assessed in parallel, bounded;
- progress lines on standard error, so a slow run is visible before it ends.

## Acceptance

- A timing test or benchmark fixture that holds the per-archive patch read to one pass.
- The profile dry run on a machine-sized archive root, measured before and after with the release
  binary, recorded as test evidence.

## Scope

Derived 2026-11-09 by `story-scoper` at 23b9c91. Every line is **cited** (read from the tree; the story names no path) or **inferred**.

- **Primary surface:** `crates/worktree-git/src/archive.rs` (`scan_sections` :2431, `walk_patch` :2290, `HeadTree::read` :2558, `observe_exclusion` :2817, `tree_ignore_files` :2671) — cited
- **Files:** `crates/worktree/src/lib.rs:672` (`strip_archive_build_output`, serial loop), `:696` (`assess_archive_strip`), `:902` (`remote_observation`) — cited
- **Files:** `crates/worktree-cli/src/main.rs:995` (`strip_build_output`, emits the report once at the end) — cited
- **Also likely:** `crates/worktree-git/src/lib.rs:1240-1290` (`begin_shared_observation`), if the remote proof is shared — inferred
- **Also likely:** `crates/worktree-domain/src/build_output.rs:1353` (`ArchiveStripReport`), if elapsed time becomes a report field — inferred
- **Tests:** new `crates/worktree-cli/tests/strip_dry_run_speed.rs`, plus unit tests in `archive.rs` (:3561 onward) — inferred
- **Documents:** `CHANGELOG.md`, `README.md`, `docs/architecture.md` — written by the coordinator at the close
- **Confidence:** medium — the code sites are read; which of them change is "decided after measuring"

Candidate changes as found on 23b9c91:

- One pass for digest and section scan: present (`walk_patch` hashes each line it hands the scanner), except a patch with `.gitignore` sections, which `tree_ignore_files` reads a second time (`archive.rs:2689`).
- Tracked-path listing cached per repository and HEAD: absent (`HeadTree::read` runs `git ls-tree -r -t` per archive).
- Remote proof shared per run: built in 0.19.1 but only `gc` calls `begin_shared_observation` (`crates/worktree/src/lib.rs:1331`); the strip reads remotes once per archive.
- Progress on standard error and elapsed time: absent.
- Nothing was measured.
