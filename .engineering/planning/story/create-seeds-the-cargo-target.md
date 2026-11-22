---
format: aep.planning-md/3
id: story:create-seeds-the-cargo-target
kind: story
status: draft
title: A new tree starts with the dependency artifacts of a built checkout
summary: the first cargo build in a new tree compiles the workspace's own crates only
relations:
- informed_by: story:recognised-build-cache-is-discarded
revision: 1
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
- Not measured: the release profile, a seed built with another rustc or other features (cargo's
  fingerprints then differ and it compiles as without a seed; expected, not run), a crate with a
  native build script, a file system with reflinks, the disk used over a day.

## Acceptance

1. With the option on and a built checkout of the same repository present, the first
   `cargo build` in the new tree runs rustc for workspace members only. A test reads the
   `Fresh` and `Compiling` lines of `cargo build -v` for a fixture workspace with one dependency
   that is not a member.
2. A member whose source differs from the seed's is rebuilt, also when its files are older than
   the copied artifacts.
3. `incremental/` is not copied, and neither are the members' own artifacts.
4. The copy uses a reflink where the file system offers one and a plain copy otherwise. It never
   makes a hard link.
5. With no built checkout, with a seed whose `.rustc_info.json` names another rustc, or with the
   option off, `create` behaves as it does today and says in one line why nothing was copied.
6. `discard-cache` classifies the seeded directory as cache, like any cargo target.
7. The tree's record names the seed's path and the bytes copied.

## Scope

- `crates/worktree-domain/src/cache.rs`, `crates/worktree-git/src/cache.rs` (the cargo target
  classifier)
- the create path of `b10x-worktree` and of the CLI, and a test beside
  `crates/worktree-cli/tests/cache_discard.rs`
