---
format: aep.planning-md/3
id: review-result:adversary-strip-ignored-pass-1
kind: review-result
status: active
title: Adversary, strip ignored output from archives, pass 1
relations:
- reviews: story:strip-ignored-output-from-archives
revision: 1
---
unit: story:strip-ignored-output-from-archives — worktree wt-unit-strip-ignored at 4b5809d (diff f7ad529...HEAD) + one new test file
verdict: red (NEEDS-CHANGE)
cases: executed 376→382, red 5
origin: introduced 5 / pre-existing 0 / undecided 0
wrote-outside-worktree: ~/.cache/worktree-wave-ignored-disposable/unit-2/adversary/{cases-alone.log,suite.log}
needs-coordinator: yes — the contract reads `.gitignore` "from HEAD, never the patch", so a patch that un-ignores a path loses it; fixing that is a spec change

(Recorded with the home directory written as ~ for the repository's personal-path scan.)

Diff: one untracked file, `crates/worktree-cli/tests/ignored_output_strip_adversary.rs`; no implementation file touched.

Cases (each run alone first):

| case (line) | asserts | state | red output |
|---|---|---|---|
| a_path_the_trees_modified_gitignore_no_longer_ignores_stays (:238) | patch removes `/target/` from `.gitignore`; the tree's Git does not ignore `target/review-1/a.md`; `sections_stripped == 0` | red | `"sections_stripped":1 … below target`, left 1 right 0 |
| a_path_a_gitignore_the_patch_adds_re_includes_stays (:253) | patch adds `logs/.gitignore` with `!keep.log`; HEAD's root rule `*.log`; 0 stripped | red | `… below logs/keep.log`, left 1 right 0 |
| a_keep_file_the_patch_adds_as_a_symlink_keeps_every_path (:268) | patch adds `.worktreekeep` as mode 120000; 0 stripped | red | `"sections_stripped":2 … below target, website/node_modules`, left 2 right 0 |
| a_binary_keep_file_in_head_keeps_every_path (:291) | HEAD's `.worktreekeep` is binary; 0 stripped | red | `"sections_stripped":2 … "prune_verdict_after":"removable"`, left 2 right 0 |
| a_dry_run_writes_nothing_beside_the_archive (:407) | archive root read-only (0555); dry-run still `strippable` | red | `"verdict":"patch-unusable" … archive-scratch-failed: …/archives/site: Permission denied … .worktree-scratch-iwBhBj` |
| the_strip_removes_exactly_what_the_trees_git_ignored (:313) | stripped paths equal `git ls-files --others --ignored --exclude-standard` in the tree (leading-colon may stay) | green | first run failed in the fixture (`archive` refuses newline names); name removed |

Suite: `cargo test --workspace --locked --no-fail-fast` → EXIT=101; 382 executed: 377 passed, 5 failed (the five above).

Findings:

| file:line | what breaks | verdict | origin | what reaches it |
|---|---|---|---|---|
| crates/worktree-git/src/archive.rs:2513 (decision build_output.rs:986) | an untracked file the tree's Git did not ignore is stripped when the patch modifies, adds or deletes a `.gitignore` that un-ignores it; the archive is its only copy | NEEDS-CHANGE | introduced | ordinary: removing `dist/` from `.gitignore` to commit generated docs, then archive and remove |
| crates/worktree-git/src/archive.rs:2301 (build_output.rs:566) | a `.worktreekeep` added as a symlink has its link target read as patterns; AGENTS.md says a non-regular keep file keeps every path | NEEDS-CHANGE | introduced | a tree sharing its keep file via symlink; rare |
| crates/worktree-git/src/archive.rs:2585 | HEAD's keep file used as patterns with no text check; the patch source treats binary as unreadable | INFEASIBLE | introduced | nothing found |
| crates/worktree-git/src/archive.rs:2632 | dry-run creates `.worktree-scratch-*` beside the archive, contrary to "a dry-run writes nothing"; read-only archive root → `patch-unusable` | INFEASIBLE | introduced | nothing found |
| crates/worktree-git/src/archive.rs:2611 | `info/exclude` read at strip time, not archive time — the drift the spec cites for excluding `core.excludesFile`; the unit's own test (ignored_output_strip.rs:424) pins it | CONFIRMED | introduced | adding to `.git/info/exclude` after archiving |

Attacked and not broken: nested `.gitignore`, negation, `**`, directory-only and anchored patterns, symlinked `.gitignore` not followed, common-dir `info/exclude`; C-quoted, non-UTF-8, tab, quote, glob, backslash and space names; stripped patch applies over HEAD; user `core.excludesFile` (explicit and XDG) does not leak; scratch removed on refusal paths; `.git` components and patch-shown Git directories. Not tested: two concurrent prune runs.

```findings
- file: crates/worktree-git/src/archive.rs
  line: 2513
  category: acceptance
  severity: blocker
  verdict: NEEDS-CHANGE
  origin: introduced
  message: a pure addition the patch's own .gitignore change un-ignores is stripped from HEAD's rules alone, losing an untracked file the tree's Git did not ignore
- file: crates/worktree-git/src/archive.rs
  line: 2301
  category: contract-drift
  severity: warning
  verdict: NEEDS-CHANGE
  origin: introduced
  message: a .worktreekeep the patch adds as a symlink has its link target read as patterns instead of keeping every path
- file: crates/worktree-git/src/archive.rs
  line: 2585
  category: contract-drift
  severity: note
  verdict: INFEASIBLE
  origin: introduced
  message: a binary keep file in HEAD is used as patterns although AGENTS.md says a non-text keep file keeps every path
- file: crates/worktree-git/src/archive.rs
  line: 2632
  category: contract-drift
  severity: note
  verdict: INFEASIBLE
  origin: introduced
  message: a dry-run now writes scratch beside the archive for every archive with rules, contrary to "a dry-run writes nothing", and refuses under a read-only archive root
- file: crates/worktree-git/src/archive.rs
  line: 2611
  category: judgement
  severity: note
  verdict: CONFIRMED
  origin: introduced
  message: info/exclude is read as it is at strip time, the same drift the spec gives for not reading core.excludesFile
```
