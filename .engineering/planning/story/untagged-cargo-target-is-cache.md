---
format: aep.planning-md/3
id: story:untagged-cargo-target-is-cache
kind: story
status: implemented
title: A Cargo target made before Cargo's first build is still recognised as cache
summary: an untagged target/ beside a tracked Cargo.toml, holding a full profile, is classified as a tagged target
relations:
- informed_by: story:cargo-target-tmp-is-cache
scope:
- confidence: cited
  path: crates/worktree-cli/tests/cache_discard.rs
- confidence: cited
  path: crates/worktree-domain/src/cache.rs
- confidence: cited
  path: crates/worktree-git/src/cache.rs
revision: 6
transitions:
- {from: "draft", to: "proposed", at: "2026-10-07T23:14:43Z", actor: "human:timo", revision: 3}
- {from: "proposed", to: "active", at: "2026-10-07T23:14:44Z", actor: "human:timo", revision: 4}
- {from: "active", to: "implemented", at: "2026-10-08T00:00:39Z", actor: "human:timo", revision: 6, decided_on: {"recorded":{"test_result":1,"review_outcome":3,"verification":1}}}
---
## Outcome

`worktree discard-cache` and `finish --discard-cache` recognise a Cargo target that has no
`CACHEDIR.TAG` because something created `target/` before Cargo's first build there, so its profiles
and its `tmp/` are discarded as they are in a tagged target.

## Why

Cargo writes `CACHEDIR.TAG` only when it creates the target directory itself. Observed with cargo
1.99.0 on 2026-10-07: after `mkdir -p target/records`, `cargo build` left `target/` holding
`debug/`, `records/` and `.rustc_info.json` and no tag; a fresh crate's `target/` held
`CACHEDIR.TAG`. The classifier treats a directory as a Cargo target only when it carries a valid tag
(`crates/worktree-git/src/cache.rs:152`, `:161`), so such a target is retained whole. On
2026-10-07 `discard-cache` recognised nothing in 2 of 3 controller clean-ups for this reason (a task
had run `mkdir -p target/<dir>` first); both controllers fell back to `cargo clean --profile dev`.

## Specification

The comment on `worktree.cache.CacheKind` in
`.engineering/specs/worktree-inspection/domains/cache.yaml` states when an untagged directory counts
as a tagged Cargo target; validated with `ess` 0.55.0. No new variant: the existing `cargo-profile`,
`cargo-target` and `cargo-target-tmp` kinds apply.

## Contract

- An ignored real directory without a valid `CACHEDIR.TAG` is classified as a tagged Cargo target
  when all three hold: it is named `target`; a tracked `Cargo.toml` sits beside it (in its parent
  directory, the tree root included); and at least one direct child other than `tmp/` holds both
  `.fingerprint/` and `deps/` as real directories.
- Classified that way, it behaves as a tagged target, with one exception: profiles are discarded
  as `cargo-profile`, `tmp/` beside a profile as `cargo-target-tmp`, Cargo's root metadata files
  are Cargo's, and a target of nothing else goes whole as `cargo-target`; every other child is
  retained and named. The exception: a `CACHEDIR.TAG` with a bad signature is not Cargo's and is
  retained.
- When Git reports a profile itself as the ignored entry, it is discarded as `cargo-profile` when
  its parent satisfies the three conditions above.
- Still retained: the same layout under any other name, beside no tracked `Cargo.toml` (an
  untracked one does not count), with no child other than `tmp/` holding both markers, a profile
  or `target` reached through a symlink, and every case already retained today.
- Revised after adversary pass 1 (review-result:adversary-untagged-target-pass-1): `tmp/` no
  longer qualifies a target, and a bad-signature tag is kept.

## Acceptance

- In a tree with a tracked `Cargo.toml`, a `target/` made before the build, holding `debug/` (with
  `.fingerprint/` and `deps/`), `tmp/` with files and `.rustc_info.json`, is discarded whole as
  `cargo-target`; `finish --discard-cache` finishes the tree without `--archive`.
- The same target beside `target/records/` keeps `records/` and discards `debug/` and `tmp/`.
- The same layout named `build/`, beside an untracked `Cargo.toml`, or with a profile holding only
  `.fingerprint/`, is retained.
- The package tests of `b10x-worktree-cli`, `b10x-worktree-git` and `b10x-worktree-domain` pass.
