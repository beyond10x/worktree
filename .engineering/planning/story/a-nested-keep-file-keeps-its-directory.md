---
format: aep.planning-md/3
id: story:a-nested-keep-file-keeps-its-directory
kind: story
status: draft
title: A .worktreekeep in a subdirectory keeps paths under that directory
summary: nested keep files, as git reads nested .gitignore
tags:
- priority:high
scope:
- confidence: inferred
  path: crates/worktree-cli/src/main.rs
- confidence: inferred
  path: crates/worktree-cli/tests/nested_keep.rs
- confidence: inferred
  path: crates/worktree-domain/src/build_output.rs
- confidence: inferred
  path: crates/worktree-domain/src/cache.rs
- confidence: inferred
  path: crates/worktree-git/src/cache.rs
revision: 4
---
# Story: a-nested-keep-file-keeps-its-directory

## Outcome

A `.worktreekeep` in a subdirectory keeps paths under that directory, its patterns matched relative to it, as git
reads a nested `.gitignore`. A tool that keeps run state in an ignored directory of its own can mark that state as
retained without editing the repository's root files. `discard-cache`, `sweep` and `finish --discard-cache --archive`
keep the marked paths, `archive` images them, and `prune-archives --strip-build-output` never strips them from an
archive. Every reader of the keep rule reads nested keep files the same way.

## Context

- Today only the root `.worktreekeep` is read: `--exclude-from=.worktreekeep` (`crates/worktree-git/src/cache.rs:453`).
  The guidance says the same: "that file at the tree root holds Git exclude patterns"
  (`crates/worktree-cli/src/main.rs:1548`).
- On a downstream trial (2026-10-27), an agent tool kept its case records in an ignored directory. `worktree finish
  --discard-cache --archive` reported `discarded .engineering/cases (IgnoredOutput, 0 MiB)` before archiving. The
  operator had copied them first; without that copy, the run's records would have been gone.
- takt is moving its cases to `.takt/cases`, ignored by a self-written `.takt/.gitignore`, and it does not edit a
  repository's root files. Without this story, every managed tree loses its cases at cleanup.

## Acceptance

Each is a named test run by `task check`.

1. `keep::a_nested_keep_file_keeps_its_directory`: an ignored `tool/` holding `tool/.worktreekeep` = `cases/` and
   files `tool/cases/a` and `tool/cache/b`. `discard-cache` deletes `tool/cache/b` and keeps `tool/cases/a`, and the
   report names it as kept by `tool/.worktreekeep`.
2. `keep::sweep_honours_a_nested_keep_file`: the same under `sweep`.
3. `keep::finish_archives_what_a_nested_keep_file_keeps`: `finish --discard-cache --archive` writes an archive whose
   patch holds `tool/cases/a` byte for byte and verifies it.
4. `keep::an_invalid_nested_keep_file_refuses`: a nested `.worktreekeep` that is a link or unreadable refuses as
   `keep-file-invalid` and deletes nothing, as the root file does.
5. `keep::a_nested_pattern_cannot_reach_outside_its_directory`: a pattern `../x` or `/x` in `tool/.worktreekeep` keeps
   nothing outside `tool/`.

6. `keep::strip_build_output_keeps_nested_kept_paths`: an archive holding `tool/cases/a`, kept by
   `tool/.worktreekeep`, under a directory the root `.gitignore` ignores, still holds it after
   `prune-archives --strip-build-output`.

## Scope

- `crates/worktree-git/src/cache.rs` (keep-pattern collection)
- `crates/worktree-domain/src/cache.rs` (`KEEP_FILE`, report wording)
- `crates/worktree-domain/src/build_output.rs` (the strip's keep rule, `:568`)
- `crates/worktree-cli/src/main.rs` (guidance text)
- `crates/worktree-cli/tests/nested_keep.rs` (new)

## Delivery

The consumer is takt's `story:a-cases-root-survives-worktree-cleanup`. That story's end-to-end check runs takt
against this working tree's locally built `worktree` binary on `PATH`. The release follows once it passes; there is
no release of this story before that check.
