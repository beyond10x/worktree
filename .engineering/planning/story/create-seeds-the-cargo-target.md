---
format: aep.planning-md/3
id: story:create-seeds-the-cargo-target
kind: story
status: implemented
title: A new tree starts with the dependency artifacts of a built checkout
summary: the first cargo build in a new tree compiles the workspace's own crates only
relations:
- informed_by: story:recognised-build-cache-is-discarded
revision: 6
transitions:
- {from: "draft", to: "proposed", at: "2026-11-22T05:25:53Z", actor: "human:timo", revision: 4, decided_on: {"recorded":{"test_result":1}}}
- {from: "proposed", to: "active", at: "2026-11-22T05:25:53Z", actor: "human:timo", revision: 5, decided_on: {"recorded":{"test_result":1}}}
- {from: "active", to: "implemented", at: "2026-11-22T05:25:53Z", actor: "human:timo", revision: 6, decided_on: {"recorded":{"test_result":1}}}
---
## Outcome

`worktree create` can give a new tree the dependency artifacts of a built checkout of the same
repository, so the first cargo build in the new tree compiles the workspace's own crates only.

## Why

- Measured on 2026-11-22 on a 20-core workstation shared by about 15 agent sessions: 372
  worktree target directories received cargo jobs in 24 hours, and 66 % of the compile-server
  CPU the build pool recorded (7.1 of 10.8 h) fell in target directories younger than 30 minutes.
- The compile cache does not cover a new tree. sccache's key holds the working directory, and a
  crate that reads `env!("OUT_DIR")` (`serde_core`, `serde`) misses at every new path, with every
  crate that depends on it. A new checkout with a warm cache still ran 101 rustc calls (80 hits,
  14 misses) and took 64.6 s.
- Experiment, one workspace of 3 members and 98 dependency units, dev profile, incremental off,
  one rustc, all steps in one job on a busy host:
  - copying `target/debug/deps`, `target/debug/build`, `target/debug/.fingerprint`,
    `target/.rustc_info.json` and `CACHEDIR.TAG` from a built checkout: 441 MiB in 1.2 s (ext4,
    plain copy);
  - first build in the new checkout: 98 units fresh, 3 compiled (the members), 15.4 s against
    64.6 s;
  - a checkout whose sources were older than the copied artifacts and differed in one file
    rebuilt the changed crate;
  - with the members removed from the copy (`cargo clean -p`), 102 MiB less and the same 3
    units compiled;
  - after the built checkout was moved away, the seeded one built a changed member and passed
    13 tests.
- The experiment ran on cargo 1.99.1. cargo 1.100.0 lays the target directory out differently:
  a build of a one-crate workspace there has no `target/debug/deps` and no
  `target/debug/.fingerprint`; the unit lies under `target/debug/build/<package>/<hash>/`. The
  copy has to name its directories per layout.
- Not measured: the release profile, a seed built with another rustc or other features (cargo's
  fingerprints then differ and it compiles as without a seed; expected, not run), a crate with a
  native build script, a file system with reflinks, the disk used over a day.
- Measured on 2026-11-22, one workspace of 3 members and 98 dependency units, one run each on a
  busy host, seed 419 MiB in 1,439 files:
  - an empty `target/`: first build 51.8 s, 101 units compiled; seed copied (0.6 s): 9.6 s, 3
    units compiled; both end at 658 MiB per tree;
  - seed hard-linked: after one dependency was rebuilt under its old file name in the tree, 10
    of the seed's files had changed, 3 in content, with cargo 1.99.1 and with 1.100.0. Two of
    them hold the absolute path of the tree that wrote them. With the seed's files read-only,
    `cargo build` exits 101 on its lock file;
  - seed as the lower layer of an overlay in a user and mount namespace: a compile server
    outside the namespace cannot open the seed's files, a `cargo build` outside it compiles
    all 101 units, and a second namespace cannot mount the same upper directory.
- Cargo holds a lock file in the profile directory while it builds (`.cargo-lock`; cargo 1.100
  also `.cargo-build-lock` and `.cargo-artifact-lock`).

## Acceptance

1. With the option on and a built checkout of the same repository present, the first
   `cargo build` in the new tree runs rustc for workspace members only. A test reads the
   `Fresh` and `Compiling` lines of `cargo build -v` for a fixture workspace with one dependency
   that is not a member and whose package id does not hold the checkout's path (cargo rebuilds a
   path dependency in every checkout).
2. A member whose source differs from the seed's is rebuilt, also when its files are older than
   the copied artifacts.
3. Only the `debug` profile is copied. `incremental/` is not copied, and neither are the
   members' own artifacts.
4. Every file is a copy made with the system's copy call, which shares the blocks where the
   file system can. No name in the new tree is a hard link to a file of the seed. Two names
   that are one file in the seed are one file in the new tree.
5. `create` behaves as it does today and says in one line why nothing was copied: with no built
   checkout, with a seed whose `.rustc_info.json` is missing or names another rustc than the
   new tree runs, and with a seed whose profile lock is held by a running cargo.
6. `discard-cache` classifies the seeded directory as cache, like any cargo target.
7. The tree's record names the seed's path, the files and the bytes copied.
8. The copy set is tested for a target directory in the layout of cargo 1.99 and in the layout
   of cargo 1.100. Acceptance 1 and 2 run with real cargo in the pinned toolchain; a run of
   both with the other cargo is in the pull request's text.
9. The option is off by default. Without it `create` copies nothing and prints nothing new.
10. The seed is the checkout with the newest build among the repository's main checkout and
    its managed trees that passes the checks of acceptance 5.

## Scope

- `crates/worktree-domain/src/cache.rs`, `crates/worktree-git/src/cache.rs` (the cargo target
  classifier)
- the create path of `b10x-worktree` and of the CLI, and a test beside
  `crates/worktree-cli/tests/cache_discard.rs`

## Not in this story

- Profiles other than `debug`, and a target directory set by `CARGO_TARGET_DIR` or
  `build.target-dir`.
- Switching the option on for a host.
