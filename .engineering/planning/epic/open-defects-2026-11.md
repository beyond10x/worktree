---
format: aep.planning-md/3
id: epic:open-defects-2026-11
kind: epic
status: active
title: Open defects and GitHub issues, November 2026
summary: Every open GitHub issue and every open defect in the store, delivered as one pull request.
revision: 3
transitions:
- {from: "draft", to: "proposed", at: "2026-11-09T21:49:40Z", actor: "human:timo", revision: 2}
- {from: "proposed", to: "active", at: "2026-11-09T21:49:40Z", actor: "human:timo", revision: 3}
---
## Outcome

Every open GitHub issue of `beyond10x/worktree` and every open defect in this planning store is
resolved or held by a typed blocker, delivered as one pull request from one integration branch.

## Source

Operator goal, 2026-11-09, verbatim: "zero remaining issues on github, all bugs fixed from planning
store". Method, verbatim: "use /planning and /wave and /worktree:managing-worktrees - but in the end
provide 1 single PR to gh. make sure all are below 1 big epic for integration via bottoken from
./atlas".

## Holds

- `story:discard-cache-survives-overlong-paths` (GitHub #44)
- `story:gc-dry-run-reports-as-it-assesses` (GitHub #45)
- `story:rebase-head-is-not-a-lock` (defect, draft since 2026-09-04; the lock half shipped through
  `task:stale-rebase-cleanup-correction`, the marker-naming refusal and the doctor note did not)
- `story:strip-dry-run-is-fast` (defect: a 2016 s dry run that prints nothing until it ends)
- `story:archive-on-old-git` (defect, `active`; believed shipped, to be proven and closed)
- `story:skill-prints-without-out` (defect, `draft`; believed shipped, to be proven and closed)

## Not held

Drafts that describe a feature, not a defect, stay in the backlog:
`story:archived-tree-can-be-restored`, `story:organization-branch-sweep`,
`story:plugin-in-repository`, `story:worktree-diff`, `story:idle-trees-are-swept`,
`story:recognised-build-cache-is-discarded`.

## Acceptance

Issues #44 and #45 are closed by the merged pull request, every story above is `implemented` or
carries an open blocker, and `aep plan artifact validate` passes. No release is cut.
