---
format: aep.planning-md/3
id: story:cargo-unit-layout-is-recognised
kind: story
status: draft
title: A profile written by cargo 1.100 is recognised as build cache
summary: the per-unit layout of cargo 1.100 counts as a profile in every rule, and rust-toolchain.toml pins one rustc
relations:
- informed_by: story:untagged-cargo-target-is-cache
revision: 1
---
## Outcome

`worktree discard-cache`, `finish --discard-cache`, `archive` and `prune-archives
--strip-build-output` recognise a Cargo profile written by cargo 1.100 as they recognise one
written by cargo 1.99, and the repository pins one rustc for the local gate and the forge.

## Why

- cargo 1.100.0 lays a profile out differently. Read on 2026-11-22 from builds with cargo 1.100.0
  (3acc79c0c): a profile holds no `.fingerprint/` and no `deps/`. Each unit lies in
  `<profile>/build/<package>/<unit>/`, where `<unit>` is 16 lowercase hexadecimal digits, and
  holds `fingerprint/` and `out/` (a build-script run holds `fingerprint/`, `out/` and `run/`).
  20 of 20 unit directories read had a 16-digit name; 19 held `fingerprint/`, all held `out/`.
- Every rule here names a profile by `.fingerprint/` (`CARGO_PROFILE_MARKER`), and the rule for a
  target without a valid tag by `.fingerprint/` and `deps/`. A profile written by cargo 1.100 is
  therefore no profile: `discard-cache` reports it as `ignored-output`, a target without a tag is
  not recognised, and `archive` writes the target's files into `dirty.patch`.
- The forge installs `stable`, the workstation's default is 1.99.1, and the repository has no
  `rust-toolchain.toml`. `a_target_real_cargo_built_after_mkdir_is_recognised` builds with the
  cargo it runs under, so the required check `gate` has failed on every pull request since the
  forge's stable became 1.100.0; the last green run on `main` is from 2026-11-10.

## Acceptance

1. A profile holding `build/<package>/<unit>/fingerprint/` as real directories, `<unit>` being 16
   lowercase hexadecimal digits, is a Cargo profile wherever one holding `.fingerprint/` is: inside
   a tagged target, below a target-triple directory, in `archive`'s rule on disk and in a patch's
   pure additions.
2. A target without a valid tag counts as tagged through a profile holding a unit directory with
   both `fingerprint/` and `out/` as real directories, under the conditions the rule already
   has (named `target`, a tracked `Cargo.toml` beside it, the profile is not `tmp`).
3. The older layout is recognised unchanged; every existing test passes without a change to
   what it asserts about that layout.
4. None of these makes a profile: `fingerprint` or `out` as a regular file or a symbolic link, a
   `build` that is a symbolic link, a unit name that is not 16 lowercase hexadecimal digits, the
   unit layout below `tmp/`.
5. A test builds a fixture with the pinned cargo and reads the result through `discard-cache`: an
   untagged target is recognised, and a tagged one reports `cargo-target` or `cargo-profile`.
6. `rust-toolchain.toml` pins rustc 1.100.0 with `rustfmt` and `clippy`; the gate workflow and the
   release workflow install the toolchain that file names.
7. `README.md`, `AGENTS.md`, `docs/architecture.md`, the skill text in the CLI and the two domain
   files of the specification name both layouts where they name the rule, and `CHANGELOG.md` has an
   entry under `Unreleased`.

## Not in this story

- A target whose intermediate files lie outside it (`build.build-dir`): it holds no unit
  directory and is not recognised, as today.
- An archive written by an earlier release of a tree whose tagged target holds a profile in the
  1.100 layout under a name other than `debug` or `release`: the earlier release archived those
  files, this one leaves them out, so verification of that archive against the still-linked tree
  answers `archive-stale`. It refuses; it deletes nothing.
