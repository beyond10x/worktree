---
format: aep.planning-md/3
id: story:scope-flag-for-reporting-subcommands
kind: story
status: implemented
title: gc assesses only the current repository by default
summary: gc without --id assesses only the records of the repository --repo resolves to; --scope profile keeps the old profile-wide selection
scope:
- confidence: cited
  path: crates/worktree-cli/src/main.rs
- confidence: cited
  path: crates/worktree-cli/tests/gc_scope.rs
- confidence: cited
  path: crates/worktree/src/lib.rs
revision: 8
transitions:
- {from: "draft", to: "proposed", at: "2026-10-08T09:10:11Z", actor: "human:timo", revision: 6}
- {from: "proposed", to: "active", at: "2026-10-08T09:10:11Z", actor: "human:timo", revision: 7}
- {from: "active", to: "implemented", at: "2026-10-08T09:26:39Z", actor: "human:timo", revision: 8, decided_on: {"recorded":{"test_result":1}}}
---
## Outcome

`worktree gc` without `--id` assesses only the records of the repository `--repo` resolves to.
`--scope profile` restores the 0.12.1 behaviour (every record below the profile's
`workspace_root`). A foreground `gc --dry-run` in a repository finishes in seconds.

## Why

`gc --dry-run` on 0.11.0, measured 2026-10-08 in `beyond10x/worktree` with `--repo .`: 36.7 s
for 9 assessments, none of them this repository's (llm-gateway 3, ess 2, connectors 2, loom-coder
1, metaharness 1). Agents reported a median of 73 s over 16 calls and 5 hits of the 120 s tool
timeout. Each assessment refreshes remote advertisements of its record's repository, so cost grows
with the profile, not with the repository the agent works in.

## Specification

`worktree.selection.CleanupScope` in
`.engineering/specs/worktree-inspection/domains/selection.yaml`; validated with `ess` 0.56.0.

## Contract

- `gc --scope repo|profile`, default `repo`. `repo` keeps records whose `repository_root` equals
  the canonical root of `--repo`. `profile` is the 0.12.1 selection, byte for byte.
- With `--id`, the named records are assessed whatever the scope, still subject to the policy's
  workspace check.
- Recovery proof, apply revalidation and every removal gate are unchanged.
- `status` and `reconcile` are out of scope for this story.
- The generated skill states the default and the flag.

## Acceptance

- In a fixture with trees of two repositories under one profile, `gc --dry-run` in repository A
  lists only A's candidates; `--scope profile` lists both.
- `gc --dry-run` in `beyond10x/worktree` with the built binary: time before and after recorded in
  the PR.
