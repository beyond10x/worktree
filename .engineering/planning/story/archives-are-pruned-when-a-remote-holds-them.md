---
format: aep.planning-md/3
id: story:archives-are-pruned-when-a-remote-holds-them
kind: story
status: implemented
title: An archive a remote fully holds is pruned; every other archive is refused with its reason
revision: 4
transitions:
- {from: "draft", to: "proposed", at: "2026-10-08T21:11:57Z", actor: "human:timo", revision: 2}
- {from: "proposed", to: "active", at: "2026-10-08T21:11:57Z", actor: "human:timo", revision: 3}
- {from: "active", to: "implemented", at: "2026-10-08T21:33:43Z", actor: "human:timo", revision: 4, decided_on: {"recorded":{"test_result":1,"review_outcome":1}}}
---
## Outcome

`worktree prune-archives` lists every archive in scope with its size and verdict, and with
`--apply --id <dir>` deletes only archives whose every recorded commit a remote ref holds and
which hold no uncommitted state, printing the bytes freed. Every other archive is refused with the
reason, and no flag forces its removal.

## Why

On 2026-10-08 the archive root held 33G (`du -sh ~/.local/state/worktree/archives`) with `/` at
38G free, and no verb removed an archive: `gc` removes trees, and `AGENTS.md` said no command
deletes an archive. Removing an archive whose commits no remote ref holds is data loss and stays
the operator's decision, by hand.

## Specification

`worktree.archive.PruneVerdict` and `worktree.archive.ArchivePruneAssessment` in
`.engineering/specs/worktree-inspection/domains/archive.yaml`; the scope reuses
`worktree.selection.CleanupScope`. Validated with `ess` 0.56.0.

## Contract

- Verb: `worktree prune-archives [--repo <path>] [--scope repo|profile] [--id <dir>]...
  [--dry-run | --apply] [--json]`. A top-level verb, because `worktree archive [PATH]` takes a tree
  reference as its positional argument and a tree named `prune` would be ambiguous.
- Dry-run is the default. Each archive directory below the configured archive root (`<id>` and
  superseded `<id>.superseded-<n>`) gets one line: directory name, worktree id, bytes (sum of the
  regular files in it), verdict, reason. Totals: archives, bytes removable, bytes refused. Files
  directly in the archive root that are not archive directories (a `.tsv`, a `.bundle`) are not
  archives and are listed as skipped, never deleted.
- Verdicts, in this order of checking: `InvalidManifest`, `UnrecordedContent`,
  `TreeStillPresent`, `NestedRepositories`, `UncommittedState`, `RemoteProofUnavailable`,
  `CommitsNotOnRemote`, else `Removable`. Remote proof uses refs freshly advertised by the
  configured remotes of `repository_root` (the same advertised-ref source the removal proof uses;
  no local remote-tracking ref, replacement ref or graft counts), and plain ancestry: HEAD and
  every unique commit must be an ancestor of an advertised ref. Patch equivalence does not count.
- `--apply` requires `--id` (exact directory names reviewed in a dry-run, like `gc --apply`);
  without `--id` it refuses. For each id it re-assesses immediately before deleting, deletes only a
  `Removable` archive (each file the manifest names, then `manifest.json`, then the empty directory
  with a non-recursive remove; an entry appearing in between refuses and keeps the rest), and
  prints `removed <dir> <bytes>` and the total bytes freed. A refused id prints its verdict and the
  run exits non-zero after the others are processed.
- No flag, environment variable or configuration makes a refused archive removable.
- The verdict logic is I/O-free in `b10x-worktree-domain`; orchestration in `b10x-worktree`; the
  CLI only parses and renders. JSON output uses the existing protocol envelope (version 4) with a
  new payload; no existing payload changes.
- `AGENTS.md` invariants: "no command deletes an archive" becomes "only `prune-archives --apply`
  deletes an archive, and only under its rule"; the generated skill (`worktree skill`) names the
  verb. README and `docs/architecture.md` describe it.

## Acceptance

Named tests in `crates/worktree-cli/tests/prune_archives.rs`, each against a real Git repository
with a bare remote:
- `removable_archive_is_listed_then_removed_with_bytes_freed`: commits pushed, no patch, tree
  removed; dry-run lists `Removable` with its bytes and deletes nothing; `--apply --id` removes it
  and prints the bytes.
- `unpushed_commit_is_refused`: `CommitsNotOnRemote`, also under `--apply --id`; archive intact.
- `patch_equivalent_commit_is_still_refused`: the same change on the remote under another commit id.
- `uncommitted_state_is_refused` and `nested_repository_image_is_refused`.
- `present_tree_is_refused`: the registered tree path still exists.
- `offline_remote_is_refused`: remote URL unreachable gives `RemoteProofUnavailable`.
- `unrecorded_file_is_refused`: an extra file in the archive directory.
- `apply_without_id_refuses`; `scope_repo_lists_only_this_repository`;
  `loose_files_are_skipped_not_deleted`; `superseded_archive_is_assessed`.
