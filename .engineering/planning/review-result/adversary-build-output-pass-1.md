---
format: aep.planning-md/3
id: review-result:adversary-build-output-pass-1
kind: review-result
status: active
title: Adversary, build output left out of archives, pass 1
relations:
- reviews: story:build-output-is-left-out-of-archives
revision: 1
---
needs-revision

```
unit: story:build-output-is-left-out-of-archives, commit 306b393 in tree unit-build-output
verdict: NEEDS-CHANGE (strip drops the new-file half of a type change of a tracked path)
cases: executed 10->12, red 2
origin: introduced 2 / pre-existing 0 / undecided 0
```

Cases in `crates/worktree-cli/tests/build_output.rs` (committed in aea2656):

| Case | Now |
|---|---|
| `strip_keeps_a_tracked_file_replaced_by_a_symlink_under_target` | red at 306b393, green after aea2656 |
| `archive_leaves_layout_out_beside_a_tracked_file_under_target` | red at 306b393, green after aea2656 |

Findings and what became of them:

1. Blocker, fixed in aea2656: strip removed the `new file mode` half of a type change of a tracked
   path and kept the deletion. Strip now removes only pure additions: a path no other section
   names and HEAD's tree (read from Git, or from the bundle) does not track.
2. Fixed in aea2656: a tracked file under a target stopped the target being recognised, so nothing
   was left out. The archive path recognises the target on its own (`cache::archive_target`);
   `discard-cache` rules unchanged.
3. Fixed in b64f787 and aea2656: a patch-side target needs a Cargo-signed `CACHEDIR.TAG` or a
   profile's `.fingerprint/`; `tmp/` counts only beside a profile. On disk an unsigned tag is kept.
4. Fixed in aea2656: the `archive-stale` refusal for layout below an unrecorded target had no test;
   `gc_refuses_layout_below_a_target_the_archive_did_not_record` now holds it.
5. Not changed: a crash between replacing the patch and renaming the manifest leaves an archive
   strip refuses as `PatchUnusable` and prune keeps as `UncommittedState`; the safe direction. No
   recovery fits the rule that an archive directory holds only the files its manifest names.

Attacked and not broken: C-quoted paths and patch-header text inside file content, a symlink swap
of `target/debug` before `gc --apply`, a new non-layout file under a target before removal, and
the cross-target `<target>/<triple>/<profile>/` layout.
