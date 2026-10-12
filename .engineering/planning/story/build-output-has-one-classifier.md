---
format: aep.planning-md/3
id: story:build-output-has-one-classifier
kind: story
status: active
title: Archive, discard-cache and the strip share one build-output classifier
revision: 3
transitions:
- {from: "draft", to: "proposed", at: "2026-10-12T17:37:56Z", actor: "human:timo", revision: 2}
- {from: "proposed", to: "active", at: "2026-10-12T17:37:56Z", actor: "human:timo", revision: 3}
---
## Outcome

`archive`, `finish --archive`, `discard-cache` (and `finish --discard-cache`, `sweep`) and
`prune-archives --strip-build-output` recognise build output by one classifier: cargo's layout, as
today, and a Flutter app's `build/` and `.dart_tool/` beside a `pubspec.yaml`. A new archive of a
Flutter app's tree leaves that output out of `dirty.patch`, and `discard-cache` deletes it.

## Why

0.16.0 recognises Flutter output only in the strip. `archive`, `finish --archive` and
`discard-cache` recognise only cargo's layout, so every new archive of a Flutter app's tree still
carries its build output in `dirty.patch`, refuses `prune-archives` as `UncommittedState`, and needs
a strip afterwards; `discard-cache` keeps the output in the tree.

## Specification

`.engineering/specs/worktree-inspection/domains/archive.yaml`: `worktree.archive.FlutterOutputKind`
(one classifier, the observation sources), `BuildOutputOrigin` (Archive covers Flutter output),
`BuildOutputTarget` (either kind of directory; verification leaves out only below recorded
directories). `.engineering/specs/worktree-inspection/domains/cache.yaml`: `CacheKind`
`FlutterOutput`. Validated with the newest `ess`.

## Contract

- One I/O-free classifier in `b10x-worktree-domain` decides whether a path is below a Flutter
  output directory, from observations its caller supplies: whether `P/pubspec.yaml` is present
  beside it as anything but a directory, and whether anything is tracked at the directory's path.
  The strip, `archive` and `discard-cache` call it; none keeps a rule of its own.
- On disk (`archive`, `discard-cache`): `P/pubspec.yaml` is a regular file (tracked or not),
  `P/<name>` is a real directory, and neither HEAD's tree nor the index holds anything at or below
  `P/<name>`. In a patch (the strip): unchanged from 0.16.0.
- `archive` leaves out each file neither HEAD nor the index tracks strictly below such a directory,
  and records the directory as a `build_output` target with origin `archive`. The outermost of a
  cargo target and a Flutter output directory is recorded. No manifest field is added; format 3 is
  unchanged on the wire.
- Removal through a format 3 archive re-observes each left-out file immediately before deleting it:
  still a file or symlink below the same recorded directory, still recognised. Anything else
  refuses.
- Verification leaves out build layout only below the directories the manifest records, so a
  format 3 archive written by 0.16.0 of a tree holding Flutter output still verifies, and removal
  through it still works.
- `discard-cache` deletes an ignored real `P/build/` or `P/.dart_tool/` that the classifier
  recognises, as `CacheKind::FlutterOutput` (JSON `flutter-output`), and retains every other
  ignored entry, a `build/` with no `pubspec.yaml` beside it included.
- Cargo behaviour is unchanged.

## Acceptance

Named tests:
- domain (`crates/worktree-domain/src/build_output.rs`):
  `one_classifier_decides_flutter_output_for_disk_and_patch`,
  `a_build_directory_with_no_pubspec_beside_is_not_flutter_output`;
- CLI against a real Git repository with a bare remote:
  `archive_leaves_flutter_output_out_beside_a_pubspec`,
  `archive_keeps_a_build_directory_with_no_pubspec_beside`,
  `archive_keeps_a_tracked_build_directory_beside_a_pubspec`,
  `finish_archive_removes_a_tree_through_left_out_flutter_output`,
  `an_archive_written_before_flutter_was_recognised_still_verifies`,
  `discard_cache_deletes_flutter_output_beside_a_pubspec`,
  `discard_cache_retains_a_build_directory_with_no_pubspec_beside`.
