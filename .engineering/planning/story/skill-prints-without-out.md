---
format: aep.planning-md/3
id: story:skill-prints-without-out
kind: story
status: draft
title: Reading the skill leaves no file in the checkout
revision: 1
---
## Outcome

`worktree skill` with no `--out` prints the generated skill to standard output and writes nothing.
Files are written only below an explicit `--out <dir>`.

## Why

The installed agent guidance said that outside the plugin `worktree skill` renders the skill, and
gave no `--out`; the default wrote `.agents/skills/worktree/` below the working directory. On
2026-10-07, 8 other repositories' primary checkouts carried an untracked copy with the generator
marker, and 2 of them were rewritten that day.

## Contract

- No `--out`: `SKILL.md` on standard output, exit 0, no file written (`worktree.guidance.SkillRender`
  with `destination: StandardOutput`, `written_files: 0`).
- `--check` and `--force` require `--out`; `--json` without `--out` is refused as
  `operation-failed`.
- `--out <dir>` writes `SKILL.md` and `agents/openai.yaml` with the JSON protocol 4 envelope
  unchanged.
- The installed guidance block says the command prints the skill and writes no file.

## Acceptance

- `worktree skill` in an empty directory prints the skill and leaves the directory empty.
- `worktree --json skill` in an empty directory is refused and leaves it empty.
- `worktree skill --out rendered` then `--check` succeed.
- `task check` exits 0.
