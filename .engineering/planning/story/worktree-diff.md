---
format: aep.planning-md/1
id: story:worktree-diff
kind: story
status: draft
title: 'worktree diff: the commits, file list and patch a managed worktree adds over its base'
summary: 'One read-only verb showing what a managed worktree holds on top of the revision it was created from: commits, per-file stat, patch, optional path filter, JSON form.'
revision: 1
---
# Story: `worktree diff` — what a managed worktree holds that its base does not

## Goal
One command answers, for a managed worktree, *what did this tree add on top of the revision it was
created from* — the commits, each commit's file list with insertion/deletion counts, and the diff
of those commits, optionally narrowed to a path — so an operator or an agent reviewing a hand-off
does not assemble it from `git show --stat`, `git diff <base>..HEAD` and a path filter by hand.

The operator's own assembly of it, 2026-09-04, which this command replaces:

```
cd <managed path>
git show <commit> --stat --format='%h %s' | tail -20
git show <commit> -- crates/entity-store/src/file.rs | grep -E '^[-+]' | grep -vE '^(\+\+\+|---)'
```

## Shape
- `worktree diff [PATH] [--base <rev>] [--path <p>]... [--stat] [--json]`.
- Default base: the revision recorded in the registry at `create` (`--base` overrides); default
  range `<base>..HEAD`.
- `--stat` prints commits and per-file counts only; without it, the unified diff follows the stat.
- `--json` emits the same as data: commits (hash, subject, author, committer), files (path,
  insertions, deletions, status), and the patch text per file.
- A worktree whose HEAD equals its base prints that and exits 0; an unmanaged path is refused by
  name, like every other verb.

## Acceptance
- On a worktree with two commits over its base, `worktree diff --stat` lists both commits and every
  changed file with counts; `worktree diff --path <one file>` prints only that file's patch.
- `--json` round-trips through the project's stable-output tests like `list --json`.
- The command reads only; it never fetches, checks out or writes.
