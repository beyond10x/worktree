---
format: aep.planning-md/1
id: story:scope-flag-for-reporting-subcommands
kind: story
status: draft
title: Scope reporting and cleanup subcommands to the repository by default
summary: gc and status report every record in the whole workspace profile, so add a --scope flag with repo, profile and global, and default to repo.
revision: 1
---
## Problem

`worktree gc --repo <path>` and `worktree status` both report far more than the repository the
operator is working in, and nothing in their output marks the difference.

`--repo` is documented as "Repository used to select its activated workspace policy"
(`worktree gc --help`), so it resolves a profile from `~/.config/worktree/config.toml` and then
assesses every record under that profile's `workspace_root`. Observed on 2026-09-02 with
`b10x-worktree-cli` 0.3.1:

- `worktree gc --repo /home/timo/beyond10x/agentplugins --dry-run` returned 31 assessments, 28 of
  them eligible, spanning atlas, connectors, aep, ess, harness, todo, website, service-sdk and
  devcenter. One belonged to agentplugins.
- `worktree gc --repo /home/timo/babelforce/projects/sbf/acd --dry-run` returned 1 assessment, and
  that record's `repository_root` was `/home/timo/babelforce/projects/devcenter` — a different
  repository inside the same `babelforce` profile.
- `worktree status --help` lists one option, `--json`. It returned all 297 records across the
  `b10x`, `babelforce` and `default` tree roots.

An agent following the generated skill's own step — run the dry-run and inspect every result —
therefore reads a list dominated by other repositories' trees. An `--apply` issued without exact
`--id` values would remove another repository's work.

## Proposal

1. Add a `--scope` flag with three values:
   - `repo` — only records whose `repository_root` is the resolved repository.
   - `profile` — every record under the resolved profile's `workspace_root`. This is today's
     behaviour.
   - `global` — every record in the registry, across every profile.
2. Default `--scope` to `repo` when the command is invoked from inside a repository, so the common
   case narrows without a flag.
3. Apply it to `gc`, `status` and `reconcile` at least.
4. Leave `--repo` meaning what it means today — profile resolution. `--scope` decides how much of
   that profile is reported.

## Acceptance

- `worktree gc --dry-run` from inside a repository assesses only that repository's records.
- `--scope profile` reproduces today's output byte for byte on the same registry.
- `--scope global` on `status` returns every record, matching today's unfiltered `status`.
- `worktree status --scope repo` in a repository with no managed trees returns an empty set rather
  than the whole registry.
- The generated skill states the default and the flag.

## Evidence

Worktree 0.3.2 documents the current behaviour in the generated skill
(`crates/worktree-cli/src/main.rs`, "Finish and clean up" step 3 and "Audit and recovery"). It
records the limitation; it does not change it.
