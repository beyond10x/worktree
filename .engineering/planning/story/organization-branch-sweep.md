---
format: aep.planning-md/3
id: story:organization-branch-sweep
kind: story
status: draft
title: One reviewed command cleans every repository's branches and idle trees
summary: branches plan / branches apply apply the organization's clean-up rules with caller-supplied publish and pull-request adapters
relations:
- informed_by: story:idle-trees-are-swept
scope:
- confidence: inferred
  path: crates/worktree-cli/src/main.rs
- confidence: inferred
  path: crates/worktree-cli/tests/branch_sweep.rs
- confidence: inferred
  path: crates/worktree-domain/src/branches.rs
- confidence: inferred
  path: crates/worktree-git/src/branches.rs
- confidence: inferred
  path: crates/worktree/src/lib.rs
revision: 2
---
## Outcome

One command plans the branch and tree clean-up of every repository in the workspace, and one
reviewed apply carries it out, so clean-up stops needing one agent session per repository and loses
no work.

## Why

On 2026-10-07, 17 agent sessions applied the same clean-up rules by hand, one per repository. The
rules are mechanical; only the publishing identity and the open pull-request lookup come from
outside the repository.

## Specification

`worktree.branches.BranchSweepItem`, `worktree.branches.BranchDecision` and
`worktree.branches.BranchAdapters` in `.engineering/specs/worktree-inspection/domains/branches.yaml`,
validated with `ess` 0.55.0. The argv templates are typed as String there (an `UNMAPPED:` marker
records that a list type is open).

## Placement

A new verb, `worktree branches plan` and `worktree branches apply --plan <file>`, not `worktree
sweep`. Sweep composes only `discard-cache` and `archive` and runs from a timer without review
(AGENTS.md invariants); branch deletion and remote writes need the reviewed-apply shape GC uses:
a plan that records what it observed, and an apply that revalidates every item and refuses one
whose tip moved.

## Rules (the plan decides, per repository in the workspace profile, after `git fetch --prune`)

1. Record every local tip in `<archive root>/<repository>/branch-tips-<date>.tsv` (branch, tip,
   upstream) before anything is deleted; apply refuses when the file is missing or differs.
2. Local branch: `DeleteMerged` (`git branch -d`) when merged into `origin/main`;
   `DeleteEquivalent` (`-D`) when `git cherry origin/main <branch>` shows only `-` lines;
   `DeleteRemoteHeld` (`-D`) when the tip is reachable from a remote-tracking ref; otherwise
   `PushThenDelete`: push unchanged through the publish adapter, then `-D`; if the adapter refuses,
   `BundleThenDelete`: write `<archive root>/<repository>/<branch>.bundle`, verify it with
   `git bundle verify` and its head equal to the tip, then `-D`.
3. Remote branch: `DeleteRemote` through the delete adapter when its work is on main (tip an
   ancestor of `origin/main`, tree equal to a `main` commit's tree, or every patch equivalent), unless
   an open pull request (pull-request adapter) or a live tree uses it.
4. A tree is live when a process has its working directory inside it (`/proc/*/cwd`) or a file in
   it changed within the liveness window (6 hours by default). A live tree is left alone and the
   branch it has checked out is `Keep`. Non-live managed trees are handled by the existing commands
   the plan lists (`finish --discard-cache --archive`, then `gc` on reviewed ids); the verb adds no
   new tree-removal path.
5. The primary checkout's current branch and `main` are always `Keep`.

## Needs from outside this repository

- A publish adapter and a remote-delete adapter: argv templates run without a shell under the
  repository's publishing identity. In this organization that is the bot route
  (`b10x-gates bot --repository <owner>/<repo> --policy <policy> -- push …`); this repository
  names no bot.
- A pull-request adapter: a read-only argv template printing open pull-request numbers for a head
  branch (in this organization `gh pr list --head <branch> --state open --json number`).
- Both come in an adapter file, a new versioned surface `worktree.branch-adapters/1`, because
  configuration version 1 is immutable.
- The daily timer that runs `plan` and the reviewer of its output stay outside this repository.

## Acceptance

- In a fixture workspace of two repositories, `branches plan` assigns each of the seven decisions
  to the branch built for it and writes the tips file before anything is deleted.
- `branches apply` deletes exactly the planned branches; a branch whose tip moved after the plan is
  refused and kept; a push the adapter refuses produces a verified bundle before `-D`.
- A remote branch with an open pull request, and one checked out in a tree whose file changed an
  hour ago, are `Keep`.
- No adapter configured: `plan` runs and marks every push or remote delete `Keep` with the reason.
- `task check` exits 0.

## Out of scope

Building it; this story only plans it. Running it across the organization. Changing `sweep`.
