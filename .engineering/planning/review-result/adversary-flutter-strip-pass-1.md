---
format: aep.planning-md/3
id: review-result:adversary-flutter-strip-pass-1
kind: review-result
status: active
title: Adversary, Flutter build-output strip, pass 1
relations:
- reviews: story:flutter-build-output-is-stripped
revision: 1
---
approve

```
unit: story:flutter-build-output-is-stripped, commits ce30c0f..93703a4 in tree unit-flutter-strip
verdict: no data loss found; 3 notes
cases: executed 296->298, red 1 (a note, not a defect)
origin: introduced 3 / pre-existing 0 / undecided 0
```

Cases in `crates/worktree-cli/tests/flutter_strip_adversary.rs` (committed in 2410a9f):

| Case | Now |
|---|---|
| `adversary_flutter_strip_removes_only_output_and_leaves_a_usable_archive` | green |
| `adversary_flutter_domain_edges_hold` | green |
| `adversary_flutter_output_inside_a_cargo_target_is_recorded_by_its_own_rule` | red as first written (asserted one outer target), green after 90bc6f4 asserts the nested record |

Findings and what became of them:

1. Note, wording fixed in 90bc6f4: a Flutter output inside a recognised cargo target, on a path
   that is not cargo layout, is recorded as its own target nested in the cargo target. Totals and
   the strip itself are right. The contract now says the outermost directory whose rule strips the
   path is recorded.
2. Note, wording fixed in 90bc6f4: the outcome promised that a source directory named `build` is
   never stripped; recognition reads HEAD only, as the cargo strip does, so an uncommitted
   `build/` beside a `pubspec.yaml` is output. The outcome now says a `build` HEAD tracks.
3. Note, wording fixed in 90bc6f4: a `pubspec.yaml` HEAD holds as a symlink or gitlink names an
   app root; specification, AGENTS.md, README and changelog now say "anything but a directory".

Attacked and not broken: a tracked `build/` (partly, fully, as a file, symlink or gitlink), a
`pubspec.yaml` only in an ancestor or a child, held as a directory, deleted, modified, purely
added or named twice, the tree-root app and nested roots, C-quoted and non-UTF-8 paths, a path
named twice, a section that is exactly `P/build`, a cargo target inside `build/`, the stripped
patch applied over HEAD, `worktree_tree`, the totals, the prune verdict and a second strip.

Outside this change, seen while building a fixture: `gc --apply` refuses a tree in which a tracked
file became a directory (`archive-unsupported-entry`) although `archive` wrote a format 1 archive
for it.
