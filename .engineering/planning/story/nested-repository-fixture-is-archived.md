---
format: aep.planning-md/3
id: story:nested-repository-fixture-is-archived
kind: story
status: active
title: A tree holding a nested Git fixture is archived and retired without loss
summary: worktree archive keeps a byte image of each nested repository, and gc retires the tree on it
refs:
- provider: github
  reference: beyond10x/worktree#23
relations:
- informed_by: story:archived-tree-can-be-retired
scope:
- confidence: cited
  path: Cargo.lock
- confidence: cited
  path: Cargo.toml
- confidence: cited
  path: crates/worktree-cli/tests/nested_archive.rs
- confidence: cited
  path: crates/worktree-domain/src/archive.rs
- confidence: cited
  path: crates/worktree-git/Cargo.toml
- confidence: cited
  path: crates/worktree-git/src/archive.rs
- confidence: cited
  path: crates/worktree-git/src/hidden.rs
- confidence: cited
  path: crates/worktree-git/src/lib.rs
- confidence: cited
  path: crates/worktree/src/lib.rs
revision: 4
transitions:
- {from: "draft", to: "proposed", at: "2026-10-07T08:09:40Z", actor: "human:timo", revision: 3}
- {from: "proposed", to: "active", at: "2026-10-07T08:09:40Z", actor: "human:timo", revision: 4}
---
## Outcome

A finished tree whose untracked or ignored files hold a nested Git repository, such as a test
fixture kept in an ignored evidence directory, is archived with a byte image of that repository,
and a reviewed `worktree gc --apply` retires it without losing any of the repository's state.

## Why

https://github.com/beyond10x/worktree/issues/23 (2026-10-03): two finished trees kept nested
fixtures under ignored `.engineering/drafts/…/test-tmp/` directories. `worktree archive` refused
both with `archive-unsupported-entry` (`crates/worktree-git/src/archive.rs:245-249`) and removal
refuses any `.git` below the root as `worktree-hidden-state`
(`crates/worktree-git/src/hidden.rs:104-128`), so neither tree can ever be retired. Git lists such
a directory only as `<path>/` and indexes nothing below it, so the existing patch cannot hold it.

## Specification

`worktree.archive.Archive` and `worktree.archive.NestedRepositoryImage` in
`.engineering/specs/worktree-inspection/domains/archive.yaml`, validated with `ess` 0.55.0.

## Contract

- A nested repository is a directory `git ls-files -z --others` (empty index) lists as `<path>/`
  whose `<path>/.git` is a real directory.
- Still refused as `archive-unsupported-entry`, naming the path and the reason: a `.git` that is a
  file or a symlink (submodule or linked worktree); a `.git` holding `commondir`, a non-empty
  `objects/info/alternates`, or any entry under `worktrees/`; a path HEAD records as a submodule
  (mode 160000); any entry below the root that is not a regular file, directory or symlink; any
  path containing a newline.
- The archive holds one `nested-<n>.tar` per nested repository, `n` counting from 1 in path-byte
  order: the root directory and every entry below it, `.git` included, with permission bits,
  modification times and symlink targets; entry names are relative to the tree root, so
  `tar -xf nested-<n>.tar -C <tree>` restores it.
- Each image has a fingerprint: the SHA-256 of the canonical listing of every entry below the root
  (relative path, kind, permission bits, size, SHA-256 of a file's content or a symlink's target),
  sorted by path bytes. The image read back, and the directory on disk before and after writing,
  must all produce the recorded fingerprint, or the archive is refused
  (`archive-verification-failed`, `archive-stale`) and nothing is published.
- The manifest is `worktree.archive/2`, adding `nested_repositories` (path, image file with its
  SHA-256 and size, fingerprint, entry count), only when at least one image exists. Every other
  archive is still written as `worktree.archive/1`, byte for byte as before. Readers accept both
  and refuse a `/1` manifest that names images.
- `worktree_tree` and `dirty.patch` cover the tree outside every imaged root.
- Verification for removal also requires every image file's recorded SHA-256, the same set of
  nested roots on disk as the manifest names, and each root's fingerprint unchanged; otherwise
  `archive-stale` or `archive-digest-mismatch`.
- `worktree-hidden-state` still refuses a nested `.git`, except below a root the verified archive a
  removal relies on images. Every other hidden-state rule is unchanged.
- GC apply removes an imaged root only after re-hashing each entry against the image's listing
  immediately before deleting it; a mismatch stops with `archive-stale` and the archive is kept.
- No command deletes an archive or an image.

## Acceptance

- Restore test: a tree whose ignored directory holds a nested repository with a commit, a staged
  change, an unstaged edit, an untracked file and a stash is archived with `finish --archive`;
  extracting the image into an empty directory reproduces the same listing, `rev-parse HEAD`,
  `status --porcelain=v2`, `stash list` and a clean `fsck`.
- The same tree is eligible in `gc --dry-run` and `gc --apply` removes it; the archive stays.
- Changing one byte under the nested `.git` after archiving makes `gc --apply` refuse with
  `archive-stale` and leaves the tree.
- A nested repository with a gitfile `.git`, and one with `objects/info/alternates`, are each
  refused by `archive` with `archive-unsupported-entry`.
- An archive of a tree without nested repositories is `worktree.archive/1` and identical in shape
  to 0.11.0's.
- `task check` exits 0.
