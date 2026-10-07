---
format: aep.planning-md/3
id: story:recognised-build-cache-is-discarded
kind: story
status: draft
title: An idle tree's recognised build cache is discarded before finish
relations:
- informed_by: story:native-worktree-storage-inspection
- informed_by: story:archived-tree-can-be-retired
revision: 1
---
## Outcome

An idle tree whose only ignored content is build cache can be finished and collected without an
agent deciding what is safe to delete. `worktree discard-cache [<tree>] [--dry-run]` deletes the
ignored directories the tool recognises as cache and reports every other ignored entry as retained.
`worktree finish --discard-cache [--archive]` discards first, then finishes; with `--archive` it
first writes an archive of whatever is left, so nothing that is not cache is lost.

## Why

On 2026-10-06, 105 active trees across four workspace profiles held 49.0 GB with no tracked or
untracked change at all, only ignored build output, and 142 `worktree finish` calls in Claude
sessions over 14 days were refused as `worktree-dirty`. The `managing-worktrees` skill tells agents
to delete only build directories "owned by this task"; agents that could not establish ownership
left the trees. A manual prune of every ignored `target/`, `node_modules/` and `.venv/` by name the
same day freed 28.6 GB and deleted agent records that ESS commits cite under `target/` (for example
`target/backlog-input/…`). A directory's name does not say whether it is cache.

## Contract

- Cache is recognised by structure, never by name alone:
  - a Cargo profile directory, one holding `.fingerprint/` or `deps/`, inside a directory carrying
    a valid `CACHEDIR.TAG`, directly or below a target-triple directory or a nested tagged target;
  - `node_modules/` when it or an ancestor inside the tree holds a tracked npm, Yarn, pnpm or Bun
    lockfile;
  - a directory holding `pyvenv.cfg` beside, or at the root of, a tracked Python manifest;
  - `.pytest_cache`, `.mypy_cache` and `.ruff_cache` carrying a valid `CACHEDIR.TAG`.
- Everything else in a tagged Cargo target (`tmp/`, records written there) is retained. A Cargo
  target emptied of everything but Cargo's own metadata files is removed.
- No symlink is followed or deleted through. Every deleted path is a real directory inside the tree.
- Refused while a session lease is live, Git locks the tree, or any process other than the caller
  and its ancestors has its working directory or an open file inside the tree (`worktree-in-use`,
  naming the process ids). Where processes cannot be observed, the report says so.
- `archive` itself still never modifies the tree. `finish` without the new flags is unchanged.

## Acceptance

- A tree with a Cargo target holding a profile and `target/backlog-input/notes.md`: the profile is
  deleted, the note is kept and reported, `finish --discard-cache` refuses `worktree-dirty`, and
  `finish --discard-cache --archive` archives the note and finishes.
- `node_modules/` without a tracked lockfile, and a virtualenv without a tracked manifest, are kept.
- A tagged directory reached through a symlink is not touched.
- A process with its working directory in the tree refuses the discard and deletes nothing.
- `--dry-run` deletes nothing and reports the same classification.
- `task check` exits 0.
