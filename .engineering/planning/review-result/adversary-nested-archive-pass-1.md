---
format: aep.planning-md/3
id: review-result:adversary-nested-archive-pass-1
kind: review-result
status: active
title: Adversary, nested repository archive, pass 1
relations:
- reviews: story:nested-repository-fixture-is-archived
revision: 1
---
needs-revision

```
unit: story:nested-repository-fixture-is-archived, commit ccf4a6d (branch impl/nested-repository-fixture-is-archived) plus my uncommitted test file in the unit tree
verdict: NEEDS-CHANGE
cases: executed 168→175, red 3
origin: introduced 2 / pre-existing 0 / undecided 1
wrote-outside-worktree: none
needs-coordinator: origin of finding 3 needs a run against base a4f0ef8; I had no base worktree
```

Cases added in `crates/worktree-cli/tests/nested_archive_adversary.rs` (496 lines, untracked):

| test | asserts | now |
|---|---|---|
| `the_documented_tar_xf_restore_reproduces_every_permission_bit_the_image_holds` | the documented `tar -xf` restore gives back the listing the image holds | red |
| `a_read_only_directory_in_a_nested_repository_is_retired_or_refused_before_any_deletion` | gc either removes the tree, or refuses with the tree unchanged | red |
| `a_read_only_ignored_directory_in_an_archived_tree_is_retired_or_refused_before_any_deletion` | the same, for an ordinary ignored file with no nested repository | red |
| `awkward_entries_restore_with_tar_xpf_and_the_tree_retires` | long names and link targets, a non-UTF-8 name with a tab, an empty directory, hard links and a repository inside the nested repository restore with `tar -xpf`; gc then retires the tree | green |
| `a_tampered_image_refuses_removal_before_anything_is_deleted` | one flipped byte gives `archive-digest-mismatch`, dry-run not eligible, tree unchanged | green |
| `an_uncovered_dot_git_beside_an_imaged_root_still_refuses_removal` | a non-repository `.git` beside an imaged root still gives `worktree-hidden-state` | green |
| `a_nested_root_moved_behind_a_symlink_refuses_removal_and_touches_neither_side` | the nested root, or its parent, replaced by a symlink to an identical copy: removal refuses and neither side changes | green |

Red output:

```
`tar -xf <image> -C <dir>` did not restore the permission bits the image holds: ["open.txt: f 666 -> f 644", "shared.txt: f 664 -> f 644", "team: d 775 -> d 755"]
gc refused with archive-discard-failed after deleting part of the tree: ["evidence/run-1/fixture/.git: d 755 -> absent", … (every .git entry) …, "evidence/run-1/fixture/b.txt: f 644 -> absent", "evidence/run-1/log.txt: f 644 -> absent"]
gc refused with archive-discard-failed after deleting part of the tree: ["evidence/run-1/log.txt: f 644 -> absent"]
```

Suite: `CARGO_BUILD_JOBS=8 cargo test -p b10x-worktree-cli -p b10x-worktree-git -p b10x-worktree-domain -p b10x-worktree --locked --no-fail-fast` → EXIT=101; every binary passes except `nested_archive_adversary` (4 passed; 3 failed); 168 without it, 175 with it.

| # | file:line | what breaks | verdict | origin |
|---|---|---|---|---|
| 1 | crates/worktree-domain/src/archive.rs:52 | the documented restore `tar -xf <image> -C <tree>` applies the caller's umask, so modes 664, 666 and 775 return as 644 and 755; the spec says the same. Fix: `tar -xpf` | NEEDS-CHANGE | introduced |
| 2 | crates/worktree-git/src/archive.rs:712 | a directory without the owner write bit inside a nested root makes `remove_imaged_root` fail with `archive-discard-failed` after deleting `log.txt` and the nested `.git`; a retry can only return `archive-stale`, so gc can never retire the tree. Nothing is lost (the image holds every deleted byte). Git's removal is guarded by `make_directories_owner_writable` (lib.rs:1201, issue #16); the discard step runs before it | NEEDS-CHANGE | introduced |
| 3 | crates/worktree-git/src/archive.rs:1535 | same mechanism for ordinary ignored files: the discard deletes `log.txt`, then fails on `evidence/sealed/kept.txt`; the same code is at base line 896, not run against base | CONFIRMED | undecided |

Attacked and not broken: a tampered image; a nested root or its parent swapped for a symlink; long names, non-UTF-8 names, empty directories, hard links, repositories inside nested ones; an uncovered `.git` beside an imaged root; manifest path checks; fingerprint serialisation ambiguity; `covered_roots` only on a verified manifest; `/1` archives unchanged. Races between verification and deletion are not deterministically testable (paths re-resolved by name, no `openat`/`unlinkat`): a note only.

```findings
- file: crates/worktree-domain/src/archive.rs
  line: 52
  category: contract-drift
  severity: warning
  verdict: NEEDS-CHANGE
  origin: introduced
  message: the documented restore `tar -xf <image> -C <tree>` applies the caller's umask and does not restore the permission bits the image holds; it must be `tar -xpf`
- file: crates/worktree-git/src/archive.rs
  line: 712
  category: boundary
  severity: warning
  verdict: NEEDS-CHANGE
  origin: introduced
  message: a directory without the owner write bit inside a nested root makes gc refuse only after deleting the nested repository's .git, and the tree can then never be retired
- file: crates/worktree-git/src/archive.rs
  line: 1535
  category: boundary
  severity: note
  verdict: CONFIRMED
  origin: undecided
  message: the archived-state discard of ordinary ignored files deletes some files and then fails on a directory without the owner write bit, the same mechanism outside nested roots
```
