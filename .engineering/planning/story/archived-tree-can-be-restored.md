---
format: aep.planning-md/1
id: story:archived-tree-can-be-restored
kind: story
status: draft
title: A retired tree is restored from its archive with one command
relations:
- informed_by: story:archived-tree-can-be-retired
revision: 1
---
## Outcome

A retired tree comes back with one command: `worktree restore <archived-id> --id <new-id>` creates
a managed tree at the archived HEAD and recreates the archived uncommitted state byte for byte.

## Why

Retirement through an archive keeps everything, but bringing it back is a seven-step manual recipe
(`--no-checkout` clone, `.git/info/attributes`, bundle fetch, `git apply` under
`core.autocrlf=false`). Agents that cannot restore cheaply treat retirement as loss and keep trees.
Deferred from 0.9.0 on 2026-10-06: a linked worktree shares `info/attributes` with the primary
checkout, so the documented recipe cannot be reused there; the state has to be written from the
archived blobs directly, bypassing attribute conversion.

## Acceptance

- Restoring an archive with a commits bundle, tracked edits, untracked and ignored files reproduces
  every file's bytes and executable bit, under an `eol=crlf` attribute too.
- Restore refuses a digest mismatch and never writes into the primary checkout's refs or config.
- The restored tree is an ordinary active managed record.
