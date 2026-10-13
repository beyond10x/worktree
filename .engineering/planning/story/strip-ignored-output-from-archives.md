---
format: aep.planning-md/3
id: story:strip-ignored-output-from-archives
kind: story
status: active
title: Archives are stripped of ignored output
relations:
- depends_on: story:ignored-entries-are-disposable-unless-kept
revision: 3
transitions:
- {from: "draft", to: "proposed", at: "2026-10-13T03:21:05Z", actor: "human:timo", revision: 2, decided_on: {"recorded":{"review_outcome":1}}}
- {from: "proposed", to: "active", at: "2026-10-13T03:21:05Z", actor: "human:timo", revision: 3, decided_on: {"recorded":{"review_outcome":1}}}
---
## Outcome

`prune-archives --strip-build-output` also strips ignored output from existing archives: a
section that purely adds an untracked file whose path HEAD's exclude rules ignore and the keep
file does not keep. Archives written before discard became generic shrink to what the tree held
that Git does not call disposable.

## Why

On 2026-10-13 `~/.local/state/worktree/archives` held 25G. 22.8G of it was `dirty.patch` files,
and in patches over 50M the strip's cargo and Flutter rules reach only part of it: 4.38G was
`node_modules`, 5.14G was non-layout content under `target/`, 3.63G other ignored paths such as
`website/build` and `.docusaurus`. Story `ignored-entries-are-disposable-unless-kept` stops new
archives from taking ignored output when `discard-cache` runs first; this story reclaims the
archives already written.

## Contract

Specification: `.engineering/specs/worktree-inspection/domains/archive.yaml`
(`worktree.archive.StripVerdict`, `worktree.archive.BuildOutputOrigin` `Strip`) and
`cache.yaml` (`worktree.cache.KeepFile`).

- Every existing strip guard stays: exact `--id` under `--apply`, dry-run by default, the tree
  path absent, the repository present, the recorded patch digest, pure additions only (a
  `new file mode` section, a path no other section names and HEAD's tree holds neither as a file
  or directory nor below a file), `.git` components and Git directories the patch shows never
  stripped, the new patch verified to apply over HEAD in a scratch index, the manifest written
  last, any refusal leaving the archive byte for byte.
- A pure-addition path is ignored output when Git's exclude engine ignores it under HEAD's rules:
  every `.gitignore` HEAD's tree holds (read from Git, never the patch) and the repository's
  `info/exclude`, evaluated without reading the primary checkout's working files (for example
  `git check-ignore --no-index --stdin -z` over a scratch work tree holding only HEAD's
  `.gitignore` files). `core.excludesFile` is not read: a user's global rules may have changed
  since the archive was written, and an unrecognised path stays.
- The keep file is read from HEAD's tree, or from the patch when the patch purely adds
  `.worktreekeep` (that section stays). A kept path is never stripped.
- Each stripped ignored path is recorded as a `BuildOutputTarget` with origin `Strip`, per
  outermost ignored directory holding no kept path, or per file when no such directory exists.
  Archive format stays `worktree.archive/3`; no manifest field or protocol version changes.
- `AGENTS.md`'s strip invariant and `CHANGELOG.md` state the rule.

## Acceptance

- An archive whose patch purely adds `website/node_modules/x/index.js`, `target/review-1/a.md`
  and `notes/todo.md`, with HEAD ignoring `node_modules/` and `target/` but not `notes/`: the
  dry-run reports `Strippable` with 2 sections stripped and 1 kept; `--apply` leaves a patch
  holding only `notes/todo.md` that applies over HEAD.
- The same archive with HEAD's `.worktreekeep` holding `target/review-*/`: `target/review-1/a.md`
  stays.
- A path ignored only by the user's `core.excludesFile` stays.
- A pure addition under a `.git` component or a Git directory the patch shows stays, whatever the
  exclude rules say.
- `task check` exits 0.
