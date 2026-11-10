---
format: aep.planning-md/3
id: story:discard-cache-survives-overlong-paths
kind: story
status: implemented
title: discard-cache does not refuse on a path longer than PATH_MAX below target/tmp
refs:
- provider: github
  reference: beyond10x/worktree#44
relations:
- decomposes: epic:open-defects-2026-11
scope:
- confidence: inferred
  path: crates/worktree-cli/tests/overlong_path.rs
- confidence: cited
  path: crates/worktree-git/src/cache.rs
revision: 7
transitions:
- {from: "draft", to: "proposed", at: "2026-11-09T21:49:41Z", actor: "human:timo", revision: 2}
- {from: "proposed", to: "active", at: "2026-11-09T21:49:41Z", actor: "human:timo", revision: 3}
- {from: "active", to: "implemented", at: "2026-11-10T01:57:57Z", actor: "human:timo", revision: 7, decided_on: {"recorded":{"test_result":1}}}
---
## Defect (GitHub #44, observed on 0.19.0)

`worktree discard-cache --dry-run <tree>` on a tree whose `target/tmp/` holds a test fixture nesting
`d/` past `PATH_MAX` (4096 bytes) refuses and deletes nothing:

```
Error: keep-file-invalid: <tree>/target/tmp/.tmpR6lkCb/x/d/d/d/.../.worktreekeep cannot be read: File name too long (os error 36); it must be a regular file of Git exclude patterns, and nothing was deleted
```

The tree root's keep file is fine; the lookup is made inside the fixture. The issue names
`crates/worktree-git/src/cache.rs`, where the nested keep-file lookup maps every `symlink_metadata`
error other than `NotFound` to `keep-file-invalid`; `ENAMETOOLONG` reaches it.

## Contract

- A directory whose full path cannot be addressed (`ENAMETOOLONG`) is not a `keep-file-invalid`
  refusal of the whole discard.
- Either the walk works relative to open directories so such an entry is classified and removed, or
  the entry is retained as one that cannot be read and the rest is discarded. Which of the two is
  the implementor's choice; no deletion leaves the tree and no symbolic link is followed either way.
- `keep-file-invalid` stays the refusal for a keep file that is a symbolic link, a directory or
  unreadable at an addressable place.
- The AGENTS.md invariants on `discard-cache` hold unchanged (kept paths, repositories, mount
  points, re-observation before each deletion).

## Acceptance

- A fixture tree with a tagged Cargo target holding a profile and `target/tmp/<x>/d/d/...` nested
  past 4096 bytes: `discard-cache --dry-run` exits 0 and reports the profile as disposable;
  `discard-cache` exits 0 and the profile is gone. The test fails on the base commit with
  `keep-file-invalid`.
- If the over-long entry is deleted, nothing outside the tree is touched; if it is retained, the
  report names it as retained with the reason.
- A `.worktreekeep` that is a symbolic link at an addressable nested directory still refuses as
  `keep-file-invalid`.
- The package gates of `b10x-worktree-git` and `b10x-worktree-cli` pass.

## Scope

Derived 2026-11-09 by `story-scoper` at 23b9c91. Every line is **cited** (read from the story or the tree) or **inferred**.

- **Primary surface:** `crates/worktree-git` (`b10x-worktree-git`), the `discard-cache` keep-file walk — cited
- **Files:** `crates/worktree-git/src/cache.rs:441-516` (`keep_files`, `keep_files_above`, `collect_keep_file`, `keep_file`) — cited
- **Symbols:** `keep_file` (`cache.rs:492`; its `Err(error) =>` arm at `:505` maps `ENAMETOOLONG` to `keep-file-invalid`) — cited
- **Tests, unit:** `crates/worktree-git/src/cache.rs` `mod tests` (`:1510`; the linked-keep-file test at `:1861` must stay green) — inferred
- **Tests, CLI:** a new file under `crates/worktree-cli/tests/` (coordinator: `overlong_path.rs`, so no unit shares a test file) — inferred
- **Also likely:** `Guard::observe` (`cache.rs:311`), `allocated_bytes` (`:1354`) and `remove` (`:1401`) in the same file — inferred
- **Documents:** `CHANGELOG.md`, `AGENTS.md` — written by the coordinator at the close, not by the unit
- **Confidence:** high — the story names the file and the tree shows the exact error arm
- **Would collide with:** any unit touching `crates/worktree-git/src/cache.rs` — cited
- **Not established:** whether a retained reason needs a new report field in `crates/worktree-domain/src/cache.rs` or `crates/worktree-cli/src/main.rs`; nothing was built or run.
