---
format: aep.planning-md/3
id: review-result:adversary-nested-cache-pass-1
kind: review-result
status: active
title: Adversary, nested repositories in build cache, pass 1
relations:
- reviews: story:discard-cache-keeps-nested-repositories
revision: 1
---
needs-change, then fixed

```
unit: story:discard-cache-keeps-nested-repositories, commit 2e1a542 in tree unit-nested-cache
verdict: 4 red; 2 fixed in 22cfe8a, 2 kept as decided limits; a fifth found while fixing, fixed in bc7d38d
cases: executed 23->33, red 4 (all green after 22cfe8a, 13 after bc7d38d)
origin: introduced 2 / pre-existing 0 / undecided 2
```

Cases in `crates/worktree-cli/tests/nested_cache_adversary.rs`.

Findings:
- F1, warning: a bare repository in `target/tmp` has no `.git` entry, so `discard-cache` deleted
  the whole target with its only commit. Fixed: a Git directory (`HEAD`, `objects/`, `refs/`)
  counts as a repository.
- F2, warning: splitting `tmp/` kept a work tree's gitfile and deleted its separate Git directory.
  Fixed by the same rule.
- F3, warning: a kept fixture with a linked worktree makes `finish --discard-cache --archive`
  refuse `archive-unsupported-entry`, so the tree is not collected. Kept: nothing is lost and the
  refusal names the path (`a_target_tmp_fixture_with_a_linked_worktree_is_kept_and_finish_names_it`).
- F4, warning: an unreadable directory in `tmp/` is kept and `gc` refuses
  `worktree-state-unreadable`. Kept the same way
  (`an_unreadable_directory_in_target_tmp_is_kept_and_gc_names_it`).
- F5, blocker, found while fixing F1: a bare repository in a kept profile was left out of the
  archive as cargo layout and `gc --apply` removed it. Fixed in bc7d38d: repository content is
  never build layout in archive, verification, removal or the strip.

Held: `.git` at an untagged target's root; a repository in a triple profile and in a profile's
`build/<pkg>/out/`; a nested target inside `tmp/`; symlinks in `tmp/` (not followed, a `.git`
symlink kept); a file named `.git` in `tmp/`; a `.git` appearing after classification;
`node_modules`.
