---
format: aep.planning-md/3
id: review-result:adversary-archive-prune-pass-1
kind: review-result
status: active
title: Adversary, archive prune, pass 1
relations:
- reviews: story:archives-are-pruned-when-a-remote-holds-them
revision: 1
---
needs-revision

```
unit: story:archives-are-pruned-when-a-remote-holds-them, commit 4e2f968 in tree unit-prune-archives
verdict: INFEASIBLE (needs-change: --apply deletes a file outside the archive directory when a manifest names it)
cases: executed 145->158, red 2
origin: introduced 2 / pre-existing 0 / undecided 0
```

Cases in `crates/worktree-cli/tests/prune_archives_adversary.rs` (commit 96704ec):

| Case | Now |
|---|---|
| `bundle_file_naming_a_sibling_path_is_never_deleted` | red at 4e2f968, green after 5d0e172 |
| `bundle_file_naming_an_absolute_path_is_never_deleted` | red at 4e2f968, green after 5d0e172 |
| `manifest_named_file_that_is_a_directory_is_refused` | green |
| `symlinked_archive_and_repository_directories_are_never_followed` | green |
| `id_with_traversal_or_absolute_path_is_refused` | green |
| `local_remote_tracking_ref_does_not_count` | green |
| `replacement_ref_does_not_count` | green |
| `graft_file_refuses_remote_proof` | green |
| `unique_commit_off_remote_refuses_although_head_is_held` | green |
| `one_unreachable_remote_of_two_refuses` | green |
| `manifest_naming_another_repository_is_judged_by_that_repository` | green |
| `superseded_archive_of_a_live_tree_is_refused` | green |
| `apply_with_a_refused_id_processes_the_rest_and_exits_non_zero` | green |

Fix: `ArchiveManifest::require_format` refuses a bundle or patch file name other than
`commits.bundle` and `dirty.patch` (5d0e172). All 1228 manifests in the local archive root
already used them.

Note, not changed: an `--apply` with an unknown `--id` refuses the whole run before any other id
is processed; the safe direction.

```findings
- file: crates/worktree-git/src/archive.rs
  line: 749
  category: boundary
  severity: warning
  verdict: INFEASIBLE
  origin: introduced
  message: delete joined the manifest's unvalidated bundle.file onto the archive path, so a manifest naming ../<file> or an absolute path made --apply delete a file outside the archive directory.
```
