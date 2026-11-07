---
format: aep.planning-md/3
id: story:gitlink-directory-survives-archive-discard
kind: story
status: active
title: An archived tree with an uninitialised submodule is removable
summary: archive::discard pruned an uninitialised submodule's empty directory, Git then reported the gitlink deleted and every apply refused worktree-dirty while the dry-run said eligible; recreate the directory of every gitlink after pruning.
scope:
- confidence: cited
  path: crates/worktree-git/src/archive.rs
revision: 5
transitions:
- {from: "draft", to: "proposed", at: "2026-11-07T11:00:06Z", actor: "human:timo", revision: 4}
- {from: "proposed", to: "active", at: "2026-11-07T11:00:06Z", actor: "human:timo", revision: 5}
---
# Story: an archived tree with an uninitialised submodule is removable

## Problem (observed 2026-11-07)

- `gc --apply` over 94 reviewed ids of one repository removed 65 and refused 22 as
  `worktree-dirty`; the dry-run minutes earlier, and again afterwards with 0.19.2 and 0.19.3, said
  each of the 22 was eligible. `git status --porcelain` in each showed one line:
  ` D migrations/deps/ivr-schemas`, a gitlink (mode 160000) whose directory is absent.
- `archive::discard` returns the tree to HEAD (`read-tree --reset`, `checkout-index --force -u`
  per differing path, deletion of archived untracked files) and then prunes every empty
  directory. `checkout-index` creates an empty directory for a gitlink; the pruning removes it;
  `require_clean_unlocked` then sees the gitlink deleted and refuses. The tree is left without
  the directory, so every later apply refuses the same way, while the dry-run's archive
  verification, which compares files, keeps reporting the archive exact.

## Change

- `restore_gitlink_directories` runs after the pruning: for every `160000` entry of
  `ls-files --stage` it creates the directory. An initialised submodule never reaches this code:
  its `.git` file refuses the archive as `archive-unsupported-entry`.

## Acceptance

- An archived dirty tree whose HEAD holds a gitlink, with the gitlink's directory removed before
  the apply, is removed by `gc --apply` (CLI test
  `an_archived_tree_with_an_uninitialised_submodule_is_retired`); without the change the apply
  refuses `worktree-dirty`.
- `task check` is green.
