---
format: aep.planning-md/3
id: story:flutter-build-output-is-stripped
kind: story
status: active
title: A Flutter app's build output is stripped from archives
scope:
- confidence: cited
  path: crates/worktree-cli/tests/build_output.rs
- confidence: cited
  path: crates/worktree-domain/src/build_output.rs
- confidence: cited
  path: crates/worktree-git/src/archive.rs
- confidence: cited
  path: crates/worktree-git/src/lib.rs
- confidence: cited
  path: crates/worktree/src/lib.rs
revision: 5
transitions:
- {from: "draft", to: "proposed", at: "2026-10-12T16:33:20Z", actor: "human:timo", revision: 2}
- {from: "proposed", to: "active", at: "2026-10-12T16:33:20Z", actor: "human:timo", revision: 3}
---
## Outcome

`worktree prune-archives --strip-build-output` also strips a Flutter app's build output from an
existing archive: its `.dart_tool/` and `build/` directories, recognised only where a
`pubspec.yaml` sits beside them, so a `build` directory HEAD tracks is never stripped (recognition reads HEAD, as the cargo strip does: an uncommitted `build/` beside a `pubspec.yaml` is output).

## Why

0.15.0 recognises only cargo's layout. Archives of a Flutter app tree carry mostly Flutter build
output, `.dart_tool/` and `build/` sections, up to 0.81 GB in one `dirty.patch` and 2.7 GB across
two such archives on one machine (reported 2026-10-12). The strip leaves them, so the archive stays
`UncommittedState` and its space is never reclaimed.

## Specification

`.engineering/specs/worktree-inspection/domains/archive.yaml`: `worktree.archive.FlutterOutputKind`
and the Flutter case of `StripVerdict` and `BuildOutputTarget`, validated with the newest `ess`.

## Contract

- An app root `P` (the tree root included) is a directory for which HEAD's tree, read from Git and
  never from the patch, holds `P/pubspec.yaml` as anything but a directory, or the patch purely adds
  `P/pubspec.yaml`. That section itself is kept.
- `P/build` and `P/.dart_tool` are output directories only while HEAD's tree holds nothing at that
  path, as a file or a directory. A `build/` or `.dart_tool/` with no `pubspec.yaml` beside it is
  never one.
- Every section that purely adds an untracked file strictly below an output directory is stripped,
  whatever it holds; the existing pure-addition rule is unchanged (a path named twice, a type
  change, a tracked path, an undecodable path keep their sections).
- The outermost directory whose rule strips the path, cargo target or Flutter output, is recorded as a
  `build_output` target with origin `strip`. No manifest field is added; formats 1 to 3 are
  unchanged.
- `archive`, `finish --archive` and `discard-cache` do not change: they do not recognise Flutter
  output (a later story).
- The strip's dry run, `--apply`, refusals and the prune rule after the strip are as in 0.15.0.

## Acceptance

Named tests:
- domain (`crates/worktree-domain/src/build_output.rs`):
  `flutter_output_beside_a_pubspec_is_stripped`,
  `flutter_output_without_a_pubspec_beside_is_kept`,
  `a_tracked_build_directory_is_never_flutter_output`,
  `a_pubspec_the_patch_adds_recognises_its_app_root`;
- CLI against a real Git repository with a bare remote (`crates/worktree-cli/tests/build_output.rs`):
  `strip_removes_flutter_build_output_beside_a_pubspec`,
  `strip_keeps_a_build_directory_with_no_pubspec_beside`.
