---
format: aep.planning-md/3
id: story:build-output-is-left-out-of-archives
kind: story
status: active
title: Cargo build output is left out of archives and stripped from old ones
revision: 3
transitions:
- {from: "draft", to: "proposed", at: "2026-10-10T10:39:50Z", actor: "human:timo", revision: 2}
- {from: "proposed", to: "active", at: "2026-10-10T10:39:50Z", actor: "human:timo", revision: 3}
---
## Outcome

`worktree archive` (and `finish --archive`) leaves cargo's own build layout out of an archive and
records what it left out and its size; `worktree prune-archives --strip-build-output` removes
exactly those sections from archives written before, after which an archive whose remaining state a
remote holds is removable under the existing prune rule.

## Why

On 2026-10-10 a scan of every archive on one machine (`prune-archives --scope profile --json`, 0.14.0)
found 1393 archives, 34.71 GiB; 919 had the verdict `UncommittedState` (31.52 GiB), and inside their
`dirty.patch` files 19.18 GiB was under `target/`. Example: `archives/entity-runtime/er-54-u2`,
`dirty.patch` 1041175771 bytes, 5664 file sections, 4191 of them under `target/debug`, 79 under
`target/tmp`. `archive` stores ignored build output as uncommitted state, `prune-archives` then
refuses the archive, and nothing reclaims the space short of deleting by hand.

## Specification

`.engineering/specs/worktree-inspection/domains/archive.yaml`: the build-output record of an
archive, the strip assessment and its verdicts, validated with the newest `ess`.

## Contract

Cargo build layout. Inside a cargo target directory `T` (recognised by structure as `discard-cache`
does on disk; in an existing patch, a directory for which the patch holds `T/CACHEDIR.TAG`,
`T/.rustc_info.json` or `T/<p>/.fingerprint/…`), a path is build layout when its first component
under `T` is `debug`, `release`, `tmp`, `CACHEDIR.TAG`, `.rustc_info.json`, or a profile directory
(one holding `.fingerprint/` as a direct child; also `T/<triple>/<profile>/` for a cross-target
layout). That covers `build/`, `deps/`, `.fingerprint/`, `incremental/`, `.cargo-lock` and `*.d`
inside a profile. Every other path under `T` (for example `T/ess-conformance/`, a session's
scratch) and every other ignored directory stays in the archive. A file Git tracks is never build
layout.

New archives:
- `archive` writes `dirty.patch` and the `worktree_tree` fingerprint over the tree minus build
  layout. The manifest gains `build_output`: per target directory its path, file count and bytes
  left out, and the totals. A new manifest format `worktree.archive/3` carries it (formats 1 and 2
  stay readable and unchanged). The CLI output names what was left out and its size.
- Removal through an archive (`gc --apply`) re-observes left-out paths immediately before removal:
  each must still be build layout under the same recognised target and is then deleted as cache;
  anything else that differs from the archived fingerprint refuses as today.
- An archive with nothing left out stays at format 1 or 2, byte-identical to 0.14.0 output.

Existing archives:
- `worktree prune-archives --strip-build-output [--repo] [--scope repo|profile] [--id <dir>]...
  [--dry-run | --apply] [--json]`. Dry-run is the default and lists each archive: bytes now, bytes
  it would strip, sections stripped, what stays (sections and bytes of the patch, nested images),
  and the prune verdict the archive would have after stripping.
- Only sections that add an untracked file (`new file mode`) under build layout are stripped;
  a section whose path cannot be decoded is kept. Nested repository images are not touched.
- `--apply` requires `--id`; per id it re-assesses, writes the new patch beside the old, verifies
  that it applies over HEAD and recomputes `worktree_tree` from it, then replaces patch and
  manifest (format 3, `build_output` recorded, `patch: null` when nothing remains) with the
  manifest written last. It prints the bytes freed per archive and in total.
- It refuses, naming why, an archive whose manifest is invalid, whose directory holds unrecorded
  content, whose registered tree path still exists, or whose repository is gone; the archive is
  then untouched.
- No flag forces removal of an archive. Removal stays `prune-archives --apply --id` under the
  existing rule.

## Acceptance

Named tests in `crates/worktree-cli/tests/` against a real Git repository with a bare remote:
- `archive_leaves_cargo_layout_out_and_records_its_size`
- `archive_keeps_non_layout_files_under_target` (e.g. `target/ess-conformance/report.json`)
- `archive_without_build_output_keeps_format_2_bytes`
- `gc_removes_a_tree_whose_left_out_layout_is_still_cache`
- `gc_refuses_when_a_left_out_path_is_no_longer_build_layout`
- `strip_dry_run_lists_bytes_and_changes_nothing`
- `strip_apply_rewrites_patch_and_archive_becomes_removable`
- `strip_keeps_tracked_file_changes_under_target`
- `strip_refuses_present_tree_and_invalid_manifest`
- `strip_apply_without_id_refuses`
