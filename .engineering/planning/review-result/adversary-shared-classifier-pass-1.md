---
format: aep.planning-md/3
id: review-result:adversary-shared-classifier-pass-1
kind: review-result
status: active
title: Adversary, shared build-output classifier, pass 1
relations:
- reviews: story:build-output-has-one-classifier
revision: 1
---
needs-change, then fixed

```
unit: story:build-output-has-one-classifier, commits 8671686..40c9619 in tree unit-shared-classifier
verdict: 2 confirmed, both introduced; fixed in 27fe581
cases: executed 219->227, red 2 (both green after 27fe581)
origin: introduced 2 / pre-existing 0 / undecided 0
```

Cases in `crates/worktree-cli/tests/shared_classifier_adversary.rs` (committed in 5be7b61).

Findings:
- F1, blocker: `discard-cache` deleted an ignored Flutter `build/` whole, including a nested Git
  repository whose commits no remote held
  (`adversary_discard_cache_keeps_a_nested_repository_inside_flutter_output`). Fixed: a Flutter
  output directory with a `.git` at any depth below it is retained whole, the walk repeated
  immediately before deletion.
- F2, warning: a cargo target named `build/` beside a `pubspec.yaml` was classified by the Flutter
  rule in `archive` and by the cargo rule in `discard-cache`, so an untracked non-layout file was
  left out of the archive and deleted unarchived on removal
  (`adversary_archive_keeps_what_discard_cache_keeps_in_a_cargo_target_named_build`). Fixed: such a
  directory is classified by the cargo rule alone in all four commands
  (`FlutterFacts::cargo_target`).

Held: tracked files below `build/` (HEAD only, index only, `git add -f`, `git rm --cached`); a
symlinked, directory or deleted `pubspec.yaml`; a symlinked `build/`; non-UTF-8 paths;
reclassification between verification and deletion; nested `build/build/`; a cargo target inside
Flutter output and the reverse; a 0.16.0 cargo-only archive of a tree holding Flutter output; the
strip unchanged from 0.16.0 apart from F2; both changed fixtures still test what they claim.

Open, outside this change: a cargo target holding a nested `.git` is deleted by `discard-cache` as
before.
