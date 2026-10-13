---
format: aep.planning-md/3
id: story:ignored-entries-are-disposable-unless-kept
kind: story
status: implemented
title: Ignored entries are disposable unless the keep file keeps them
relations:
- informed_by: story:recognised-build-cache-is-discarded
revision: 5
transitions:
- {from: "draft", to: "proposed", at: "2026-10-13T02:15:30Z", actor: "human:timo", revision: 2}
- {from: "proposed", to: "active", at: "2026-10-13T02:15:30Z", actor: "human:timo", revision: 3}
- {from: "active", to: "implemented", at: "2026-10-13T03:22:12Z", actor: "human:timo", revision: 5, decided_on: {"recorded":{"test_result":1,"review_outcome":1}}}
---
## Outcome

`discard-cache` (and `finish --discard-cache`, `sweep`) treats every ignored entry Git reports as
disposable, unless the repository's keep file keeps it or it is or holds a repository. The set of
build tools the CLI knows stops deciding what is freed: an ignored entry no structural rule
recognises is discarded as `ignored-output` instead of retained.

## Why

On 2026-10-13 `~/.local/state/worktree` held 71G on a 848G disk with 19G free (`df -h /`):
46G of trees and 25G of archives. The hourly `worktree sweep --all-profiles` reported
`165 idle tree(s): 0 MiB of recognised cache` at 03:19 that day, and 158 idle trees retained
3.8G of ignored entries no rule recognises (`sweep --all-profiles --dry-run --json`,
`retained_bytes`). Archives of patches over 50M held 4.38G of `node_modules`, 3.09G of Flutter
output and 5.14G under `target/` that is not cargo layout (agent scratch such as
`target/review-boundaries-17`, `target/xtask-conform-cases`). Each ecosystem has needed its own
recogniser (cargo 0.9.0, node/venv/tool caches 0.9.0, Flutter 0.17.0), and the output of the
next one is retained and archived until it gets one. Git already knows which paths a repository
calls disposable: its exclude rules.

The operator decided on 2026-10-13 that an ignored path is disposable unless the repository lists
it in a keep file. This reverses `AGENTS.md`'s "anything unrecognised is retained": agent records
written into ignored directories (`target/backlog-input/…`, which ESS commits cite, deleted by a
by-name prune on 2026-10-06) are deleted unless the repository keeps them. Records belong outside
ignored build directories, or in the keep file.

## Contract

Specification: `.engineering/specs/worktree-inspection/domains/cache.yaml`
(`worktree.cache.CacheKind` `IgnoredOutput`, `worktree.cache.KeepFile`).

- The keep file is `.worktreekeep` at the tree root, read from disk. Absent: nothing is kept. A
  symbolic link, a directory or an unreadable file there refuses the discard as
  `keep-file-invalid` and deletes nothing.
- Its lines are Git exclude patterns, matched by Git's own exclude engine (for example
  `git ls-files -z --others --ignored --exclude-from=<keep file>`, argv only), so negation,
  directory-only patterns and anchoring mean what they mean in `.gitignore`. A path is kept when
  a pattern matches it or one of its ancestors.
- Every ignored entry Git reports (`status --ignored=matching`, file or directory) is classified:
  - kept, a repository (the existing walk: `.git` entry or Git directory), on another device than
    the tree root, or not readable: retained;
  - a directory holding a kept path or a repository at any depth: never deleted whole; each direct
    entry is classified by this same rule;
  - otherwise discarded. The existing structural kinds still name what they recognise
    (`cargo-profile`, `node-modules`, …); everything else is `ignored-output`.
- A symbolic link is deleted itself and never followed. Every deletion is inside the tree.
- The walk for kept paths and repositories is repeated immediately before each deletion; an entry
  that is or holds one then is retained (and, for a directory, falls to its parts on the next run).
- Unchanged: refusal for a live lease, a Git lock or another process using the tree; `--dry-run`
  deletes nothing and reports the same classification; `archive` never modifies the tree;
  `sweep` never removes a tree.
- `AGENTS.md`'s `discard-cache` invariant, the module docs in `crates/worktree-domain/src/cache.rs`
  and `crates/worktree-git/src/cache.rs`, `CHANGELOG.md` and the generated skill (`worktree skill`)
  state the new rule. CLI JSON protocol stays 4: `ignored-output` is a new kind value, as
  `flutter-output` was in 0.17.0.

## Acceptance

- A tree with ignored `target/backlog-input/notes.md`, `website/node_modules/` without a tracked
  lockfile, ignored `app.log` and no keep file: all three are discarded; `target/backlog-input`
  and `app.log` are reported as `ignored-output`.
- The same tree with `.worktreekeep` holding `target/backlog-input/`: the note is retained,
  every other entry of `target/` is discarded, and `finish --discard-cache` refuses
  `worktree-dirty` while `finish --discard-cache --archive` archives the note and finishes.
- Negation follows Git's exclude engine. Corrected 2026-10-13: the first draft said
  `!target/backlog-input/keep.md` after `target/backlog-input/` releases `keep.md`; Git cannot
  re-include a path whose parent directory is excluded, so `keep.md` stays kept there (implementor
  probe with git 2.55.0, test `a_negation_below_an_excluded_directory_keeps_what_git_keeps`).
  With `target/backlog-input/*` followed by the negation, `keep.md` is discarded and the rest kept
  (test `a_negated_keep_pattern_discards_the_one_path_it_names`).
- An ignored directory holding a nested repository at depth 3 is not deleted whole; its other
  direct entries are discarded.
- An ignored symbolic link to a directory outside the tree is deleted; the directory it points to
  is untouched.
- A `.worktreekeep` that is a symbolic link refuses `keep-file-invalid` and deletes nothing.
- `--dry-run` deletes nothing and reports the same classification as the apply.
- `task check` exits 0.
