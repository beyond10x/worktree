---
format: aep.planning-md/3
id: story:strip-dry-run-is-fast
kind: story
status: draft
title: A profile-wide archive dry run finishes in a foreground call
revision: 1
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
