---
format: aep.planning-md/3
id: review-result:adversary-ignored-disposable-pass-1
kind: review-result
status: active
title: Adversary, ignored entries disposable unless kept, pass 1
relations:
- reviews: story:ignored-entries-are-disposable-unless-kept
revision: 1
---
unit: story:ignored-entries-are-disposable-unless-kept (wt-unit-ignored-disposable at f7ad529 + one untracked test file)
verdict: red
cases: executed 369→373, red 4
origin: introduced 4, pre-existing 0, undecided 0
wrote-outside-worktree: ~/.cache/worktree-wave-ignored-disposable/unit-1/adversary/{suite.log,suite-nofailfast.log}
needs-coordinator: yes — finding 3 contradicts the brief's "archive is unchanged" rule; findings 2 and 4 may need a contract decision rather than a code fix

Cases added, all in crates/worktree-cli/tests/ignored_output_adversary.rs, all four red when first run alone:

| case | asserts | red output |
|---|---|---|
| an_ignored_file_that_cannot_be_read_is_retained (:178) | ignored mode-000 files (`private.log`, `scratch/owned-by-a-container.db`) survive | `left: (false, false) right: (true, true)`; discarded `private.log`, `scratch` |
| an_empty_directory_a_keep_pattern_names_is_kept (:208) | `.worktreekeep` holding `scratch/mounts/` keeps empty `scratch/mounts/data` | `discarded: [{"path":"scratch","kind":"ignored-output"}]` |
| finish_with_archive_archives_every_kept_path (:230) | with `target/` kept, `finish --discard-cache --archive` archives `target/tmp/run-evidence.json` and `target/debug/coverage-report.txt` | `"patch":null`, `build_output: files 5, targets [target]`; run `ok:true`, tree finished |
| a_bind_mount_below_an_ignored_directory_is_not_deleted_through (:269) | a file outside the tree bind-mounted at `scratch/mnt` survives (`unshare -rm`; skips where user namespaces are not allowed) | `left: None`; stderr `cache-delete-failed … scratch: Device or resource busy` |

Suite: `cargo test --workspace --locked` stops at the first failing binary (171 cases, exit 101). With `--no-fail-fast`: 369 passed, 4 failed (the 4 above), exit 101.

Findings:

| file:line | what breaks | verdict | origin | what reaches it |
|---|---|---|---|---|
| crates/worktree-git/src/cache.rs:148 | contract and generated skill say an unreadable entry is retained; a file is unlinked regardless of permissions | NEEDS-CHANGE | introduced | an ignored file a container or another user left unreadable |
| crates/worktree-git/src/cache.rs:259 | "a path is kept when a pattern matches it": the kept set is built only from files `ls-files` lists, so a named directory holding no files is deleted | CONFIRMED | introduced | a placeholder or mount-point directory named in `.worktreekeep` |
| crates/worktree-git/src/cache.rs:306 | `cargo_targets` ignores the keep file; a kept target keeps its tag and profile, so `archive` leaves kept files under `tmp/`, `debug/` and the profile out as build output and `finish --discard-cache --archive` finishes with no patch; GC can then remove kept records | NEEDS-CHANGE | introduced | `.worktreekeep` holding `target/` |
| crates/worktree-git/src/cache.rs:127 | device check is `st_dev` only; a same-filesystem bind mount shares it, so `remove_dir_all` (:1034) deletes through it, outside the tree | INFEASIBLE | introduced for unrecognised dirs; mechanism existed for recognised cache | nothing found that bind-mounts into a tree; fix would compare mount ids or read /proc/self/mountinfo |

Attacked and not broken: symlinks (top level and inside; link deleted, target untouched; symlinked or directory `.worktreekeep` refuses); ancestor turned symlink refused; nested repositories (gitfile, symlinked `.git`, bare, separate-git-dir, at depth, appearing after classification); keep-file syntax (CRLF, BOM, ancestors, scoped re-read, literal pathspecs, negation below an excluded directory) matches git 2.55; `holds_kept` range query; tracked files under ignored dirs never reported; hard links only unlinked; permission repair does not follow symlinks; dry-run and apply share the re-check; lease and process refusals unchanged.

Session lease `adversary-ignored-disposable-u1` released.

```findings
- file: crates/worktree-git/src/cache.rs
  line: 148
  category: acceptance
  severity: warning
  verdict: NEEDS-CHANGE
  origin: introduced
  message: an ignored file this user cannot read is unlinked although the contract and the generated skill say an entry that cannot be read is retained
- file: crates/worktree-git/src/cache.rs
  line: 259
  category: contract-drift
  severity: note
  verdict: CONFIRMED
  origin: introduced
  message: an ignored directory a keep pattern names is deleted when it holds no files, because the kept set is built only from files ls-files lists
- file: crates/worktree-git/src/cache.rs
  line: 306
  category: acceptance
  severity: blocker
  verdict: NEEDS-CHANGE
  origin: introduced
  message: with target/ kept, archive still leaves the target's tmp and profile files out as build output, so finish --discard-cache --archive finishes with kept records in no archive
- file: crates/worktree-git/src/cache.rs
  line: 127
  category: boundary
  severity: warning
  verdict: INFEASIBLE
  origin: introduced
  message: a same-filesystem bind mount below an ignored directory shares the root's st_dev, so remove_dir_all deletes through it outside the tree
```
