---
format: aep.planning-md/1
id: story:idle-trees-are-swept
kind: story
status: draft
title: Idle trees lose their build cache and expired ones are archived without an agent
relations:
- informed_by: story:recognised-build-cache-is-discarded
revision: 1
---
## Outcome

`worktree sweep --all-profiles`, run daily from a timer, discards the recognised build cache of
every tree idle for a day without a live lease and archives what expired or finished trees still
hold, so a reviewed `worktree gc` can remove them without loss.

## Why

0.9.0 gave agents a safe finish, but cleanup still depended on each agent remembering it. On
2026-10-06, 314 active trees held no live lease: 103 idle under a day, 154 idle 1 to 7 days and 57
idle 7 days or more.

## Contract

- Idle time counts from the later of the registry's activity and the tree's own Git index, HEAD and
  HEAD log.
- A live lease, a Git lock or a process using the tree leaves it untouched and is reported.
- An archive above the configured retained size is refused as `archive-too-large`.
- The sweep never changes lifecycle, never removes a tree and never applies GC.

## Acceptance

- An idle expired tree loses its Cargo profile, keeps a record in `target/`, gains an archive and
  shows as eligible in `gc --dry-run`; a leased tree beside it is untouched.
- An archive over the limit is refused and nothing is written.
- `task check` exits 0.
