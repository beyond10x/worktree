---
format: aep.planning-md/3
id: story:cargo-target-tmp-is-cache
kind: story
status: active
title: Cargo's target/tmp scratch is discarded as cache whatever it holds
summary: a non-empty target/tmp no longer forces an archive of test scratch
relations:
- informed_by: story:recognised-build-cache-is-discarded
scope:
- confidence: cited
  path: crates/worktree-cli/tests/cache_discard.rs
- confidence: cited
  path: crates/worktree-domain/src/cache.rs
- confidence: cited
  path: crates/worktree-git/src/cache.rs
revision: 4
transitions:
- {from: "draft", to: "proposed", at: "2026-10-07T08:09:40Z", actor: "human:timo", revision: 3}
- {from: "proposed", to: "active", at: "2026-10-07T08:09:40Z", actor: "human:timo", revision: 4}
---
## Outcome

`worktree discard-cache` and `finish --discard-cache` delete Cargo's `target/tmp` scratch directory
whatever it holds, so a tree whose test binaries wrote there finishes without `--archive` and a
target of nothing but cache goes whole.

## Why

`target/tmp` is Cargo's `CARGO_TARGET_TMPDIR`, which integration tests and benchmarks fill with
scratch. The classifier keeps any non-empty child of a tagged target that is not a profile
(`crates/worktree-git/src/cache.rs:212-214`), so a non-empty `target/tmp` was retained and the tree
needed an archive of scratch; the existing test covers only an empty `target/tmp`
(`crates/worktree-cli/tests/cache_discard.rs:251-268`). Controller clean-ups on 2026-10-07 reported
trees keeping it.

## Specification

`worktree.cache.CacheKind` gains `CargoTargetTmp` in
`.engineering/specs/worktree-inspection/domains/cache.yaml`, validated with `ess` 0.55.0.

## Contract

- `tmp` is cache only as a real directory directly below a target directory that carries a valid
  `CACHEDIR.TAG` and holds at least one Cargo profile. Its content does not matter.
- A target holding only profiles, nested targets of that shape, Cargo metadata, empty directories
  and such a `tmp` is discarded whole as `cargo-target`. Beside retained content, `tmp` is listed
  on its own with kind `cargo-target-tmp`.
- `tmp` stays retained when it is a symlink, below a target-triple directory, in a target with no
  profile, or in a directory without a valid tag. A name alone never qualifies.

## Acceptance

- A tagged target with a profile and a `tmp/` holding files, a subdirectory and a nested Git
  repository is discarded whole; `finish --discard-cache` finishes the tree without `--archive` and
  `gc --apply` removes it.
- Beside a retained `target/backlog-input`, `target/tmp` is discarded as `cargo-target-tmp` and the
  record is kept.
- A `tmp/` in an untagged directory, in a tagged directory without a profile, and as a symlink is
  retained.
- `task check` exits 0.
