---
format: aep.planning-md/3
id: story:patch-equivalence-in-bounded-git-calls
kind: story
status: active
title: Patch equivalence costs a bounded number of Git calls per tree
summary: patch_equivalence ran two Git calls per advertised tip; against 1,608 refs one tree cost 64 s. List the tip commits touching the unique commits' paths once, read their verbatim ids once, and ask Git's cherry-pick check only of a qualifying tip.
scope:
- confidence: cited
  path: crates/worktree-git/src/lib.rs
revision: 5
transitions:
- {from: "draft", to: "proposed", at: "2026-11-07T10:34:53Z", actor: "human:timo", revision: 4}
- {from: "proposed", to: "active", at: "2026-11-07T10:34:53Z", actor: "human:timo", revision: 5}
---
# Story: patch equivalence in bounded Git calls

## Problem (observed 2026-11-07)

- `ProcessGit::patch_equivalence` ran `rev-list --right-only --cherry-pick <tip>...<head>` for
  every advertised tip, and `git log --patch` for every tip that passed, until one tip qualified.
- A repository advertising 1,608 refs made a one-tree dry-run with 5 unpublished commits cost
  64 s (`gc --dry-run --id dev-1128-migration-lock-guard`, 0.19.2); of 50 one-second samples of
  the profile dry-run, 38 showed a `--cherry-pick` child. That repository holds 84 finished and
  36 active records.

## Change

- A commit carrying a unique commit's patch verbatim touches the same paths. `touched_paths`
  lists the unique commits' paths (one `diff-tree --stdin -z`); `commits_touching` lists the
  tips' own commits touching them (one `rev-list --full-history --no-merges --stdin` over every
  tip, `^head`, `--literal-pathspecs`); `verbatim_patch_ids` reads their ids from `--stdin`
  (one call, revisions from a file); `commits_containing` names the tips holding each carrier.
- A tip qualifies when it contains a carrier of every unique commit. Qualified tips are tried in
  the same rank order as before; `rev-list <head> ^<tip>` says whether the tip lacks commits of
  HEAD beyond the unique ones, and only then is Git's `--cherry-pick` equivalence asked.
- A path holding a newline cannot be carried by `rev-list --stdin`: the proof is unproven.

## Acceptance

- `touched_paths` names the paths of the unique commits; `commits_touching` lists only the tip
  commits touching them, newest first, and nothing when the tip touches other paths.
- A tip carrying the unique commits verbatim but lacking another commit of HEAD is unproven
  until it also carries that commit, then proves as `patch-equivalent` naming that tip.
- A unique commit touching a path with a newline is unproven.
- Every earlier equivalence test holds: rebased work on main proves, a missing commit, a
  whitespace-only difference, a merge, an empty commit, a replacement object and a graft do not.
- The same one-tree dry-run takes 4 s with this change (64 s before), same verdict.
- `task check` is green.
