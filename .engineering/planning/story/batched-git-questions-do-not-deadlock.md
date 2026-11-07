---
format: aep.planning-md/3
id: story:batched-git-questions-do-not-deadlock
kind: story
status: active
title: Batched Git questions do not deadlock on their pipes
summary: output_with_stdin wrote every input line before reading a reply; with 3,316 advertised refs both pipes filled and every recovery proof against that repository hung. Write the input beside the reader.
scope:
- confidence: cited
  path: crates/worktree-git/src/lib.rs
revision: 5
transitions:
- {from: "draft", to: "proposed", at: "2026-11-07T10:11:05Z", actor: "human:timo", revision: 4}
- {from: "proposed", to: "active", at: "2026-11-07T10:11:05Z", actor: "human:timo", revision: 5}
---
# Story: batched Git questions do not deadlock on their pipes

## Problem (observed 2026-11-07)

- 0.19.1 feeds `cat-file --batch-check` and `rev-list --parents --stdin` through
  `ProcessGit::output_with_stdin`, which wrote the whole input before reading any output.
- Git answers each input line as it arrives. Both pipes hold 64 KiB. Once the replies fill the
  stdout pipe Git blocks writing, the input pipe fills, the writer blocks, and neither side
  returns.
- A repository advertising 3,316 refs (`ls-remote --refs origin | wc -l`) hit this: the
  `cat-file --batch-check` child of a profile-scope dry-run stayed the same PID across six
  one-second samples and the dry-run had produced no output after 5 h 01 min; a `gc --apply` from
  another session on the same repository showed the same child after 54 min.
- The 0.19.1 tests passed because every fixture advertises fewer refs than a pipe holds.

## Change

- `output_with_stdin` writes the input from its own thread while the parent drains the output
  with `wait_with_output`. A write error is a refusal only when Git itself succeeded; otherwise
  Git's own stderr is the refusal.

## Acceptance

- `present_commits` over 4,000 unknown objects (about 200 KiB in, 230 KiB out) returns within
  60 s and names only HEAD; with the 0.19.1 synchronous write the same test times out.
- `task check` is green.
