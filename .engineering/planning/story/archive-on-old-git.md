---
format: aep.planning-md/3
id: story:archive-on-old-git
kind: story
status: active
title: Archive works on Git before 2.42
revision: 3
transitions:
- {from: "draft", to: "proposed", at: "2026-10-14T06:38:10Z", actor: "human:timo", revision: 2}
- {from: "proposed", to: "active", at: "2026-10-14T06:38:10Z", actor: "human:timo", revision: 3}
---
## Outcome

`worktree archive` and `worktree finish --archive` write and verify an archive on every Git the
tool supports, from Git 2.31 on, Git before 2.42 included (Apple's Git 2.39 on macOS among them).
The oldest Git supported is stated in the README and `AGENTS.md`.

## Why

A user on macOS with Apple Git 2.39.2 and worktree 0.18.1 ran
`worktree finish --discard-cache --archive <tree>` and it failed with
`git-command-failed: fatal: options not supported in --stdin mode`; they finished without
`--archive` once the tree's tip was an ancestor of `main`. `write_bundle`
(`crates/worktree-git/src/archive.rs`) passes `git bundle create --stdin` the revisions
`HEAD`, `--not`, then each advertised tip. Git before 2.42 reads `--stdin` revisions in
`read_revisions_from_stdin` (`revision.c`, v2.39.2), which dies with that message on any line
that starts with `-` other than `--`; the 2.42.0 release notes say the `--stdin` mode "now allows
feeding options like --all and --not". The other `--not` (`unique_range`, `lib.rs`) is on argv,
which every Git accepts.

## Contract

- The ESS archive domain (`worktree.archive.Archive.unique_commits`) states that the bundle's
  revisions reach `git bundle create --stdin` as HEAD and one `^<tip>` line per advertised tip,
  never a `--not` line.
- `write_bundle` writes `^<tip>` lines; the bundle's content is unchanged (HEAD minus every tip).
- Git 2.31 is the floor: the oldest Git with `rev-parse --path-format=absolute`,
  `log --diff-merges=off` and `bundle create --stdin` (2.31.0 release notes), all of which the
  tool calls.

## Acceptance

- `crates/worktree-cli/tests/old_git.rs` (no libtest harness) puts a Git double first on
  `PATH` that refuses option lines on a revision `--stdin` the way Git before 2.42 does, and shows
  it refuses `--not` (exit 128, the same message); `worktree archive` through it records one
  unique commit and its `commits.bundle` passes `git bundle verify`. The test fails on 0.18.1's
  `write_bundle` and passes after the change.
- The package gates of `b10x-worktree-git` and `b10x-worktree-cli` pass on the pushed commit, and
  the pull request's CI is green.
