---
format: aep.planning-md/3
id: story:discard-cache-keeps-nested-repositories
kind: story
status: active
title: discard-cache never deletes a nested repository
revision: 4
transitions:
- {from: "draft", to: "proposed", at: "2026-10-12T20:01:34Z", actor: "human:timo", revision: 2}
- {from: "proposed", to: "active", at: "2026-10-12T20:01:35Z", actor: "human:timo", revision: 3}
---
## Outcome

`discard-cache` (and `finish --discard-cache`, `sweep`) never deletes a Git repository nested in
a tree's build cache. `finish --discard-cache --archive` then archives such a repository as a
nested repository image, and the tree is still collected.

## Why

0.17.0 deletes a recognised Cargo target, or its `tmp/`, whatever it holds. Integration tests
write fixture repositories into `CARGO_TARGET_TMPDIR`; such a repository has commits and files no
remote holds. Observed with 0.17.0 against a tagged target with a profile and
`tmp/fixture` holding one commit and no remote: `discard-cache --dry-run` lists `target` as
`cargo-target`, `archive` images `target/tmp/fixture` (`nested-1.tar`, 43 entries), and
`discard-cache` deletes the target with the repository. `finish` discards before it archives, so
`finish --discard-cache --archive` loses that commit before the archive can image it. 0.17.0
already retains a Flutter output directory holding a `.git`; Cargo targets, Node, Python and tool
caches have no such rule.

## Specification

`.engineering/specs/worktree-inspection/domains/cache.yaml`, `worktree.cache.CacheKind`: a nested
repository is never cache, whatever kind of directory holds it. No new variant, no new field,
CLI JSON protocol 4 unchanged. Validated with the newest `ess`.

## Contract

- One walk decides whether a directory holds a `.git` (file, directory or symbolic link) at any
  depth: it follows no symbolic link, and an entry whose type it cannot read or a directory it
  cannot list counts as holding one. Every cache kind uses it; the Flutter-only check goes.
- A whole Cargo target (`cargo-target`) holding one falls to its parts: each profile, nested
  target and `tmp/` is classified and discarded under its own rule, and a part holding one is
  handled as below.
- Cargo's `tmp/` holding one: each direct entry of it holding none is discarded as
  `cargo-target-tmp`; each direct entry holding one is retained.
- Every other recognised directory (profile, `node_modules`, virtual environment, tool cache,
  Flutter output) holding one is retained whole.
- The walk is repeated immediately before each deletion; a directory that holds a `.git` then is
  retained whole, whatever its kind.
- Retained entries appear in `retained_ignored` as today. `archive` images each retained
  repository; removal through the archive is unchanged.

## Acceptance

Named tests, CLI against a real Git repository with a bare remote
(`crates/worktree-cli/tests/cache_discard.rs`):
- `a_nested_repository_in_target_tmp_is_never_discarded`: a tagged target with a profile and
  `tmp/` holding scratch and a fixture repository with one commit no remote holds;
  `discard-cache` deletes the profile and the scratch, and the fixture's commit is still
  readable with `git cat-file -e`.
- `finish_discard_cache_archive_images_a_fixture_repository_and_the_tree_is_collected`: the same
  tree; `finish --discard-cache --archive` writes an archive imaging the fixture, and
  `gc --apply --id` removes the tree.
- `a_profile_holding_a_nested_repository_is_retained_whole`.
- `node_modules_holding_a_nested_repository_is_retained_whole`.
- the untagged-target tests hold the same rule for a target that counts as tagged.

## Delivered

Delivered beyond the first contract, after the adversary pass:

- A Git directory with no `.git` entry (a bare repository, a separate Git directory) counts as a
  repository: `HEAD` a regular file, `objects/` and `refs/` real directories, no symlink followed.
- `archive`, verification, removal through a format 3 archive and
  `prune-archives --strip-build-output` never treat repository content as build layout: a path
  with a `.git` component, or at or below a Git directory (on disk; in a patch, a directory whose
  `HEAD` and some `objects/` path the patch purely adds), stays archived and is never deleted as
  layout. Before, a bare repository in a kept profile was left out and `gc --apply` removed it.

Limit, decided: a tree whose kept state the archive cannot image (a fixture with linked
worktrees, a gitfile or symlinked `.git`) or gc cannot read (an unreadable directory) is not
collected. The refusal names the path (`archive-unsupported-entry`, `worktree-state-unreadable`);
the tree's owner resolves it. Deleting it would be the loss this story prevents.

Tests added for these: `nested_cache_adversary.rs` (13 cases), `build_output.rs`
`strip_keeps_a_bare_repository_beside_the_layout_it_strips`, domain
`a_repository_the_patch_adds_below_a_profile_is_never_stripped`,
`repository_content_is_a_git_component_or_below_a_git_directory`,
`a_git_directory_needs_head_objects_and_refs`, git `a_git_directory_is_recognised_by_head_objects_and_refs`.
