---
format: aep.planning-md/3
id: approval-record:wave-1-open-defects-2026-11-09
kind: approval-record
status: draft
title: Wave 1 of the open-defects plan run with no operator present
tags:
- non-interactive
relations:
- decides: story:archive-on-old-git
- decides: story:discard-cache-survives-overlong-paths
- decides: story:gc-dry-run-reports-as-it-assesses
revision: 1
---
## Which stop

The stage-1 proposal of wave 1 of 4 of the plan in `.engineering/plans/open-defects-2026-11.md`.

## Why non-interactive

The session is a coordinator sub-agent held by a Stop hook until the goal is met; no operator turn
exists. The goal brief (operator Timo, 2026-11-09): "The operator's goal is the acceptance of these
stories and the approval of each wave." Goal, verbatim: "zero remaining issues on github, all bugs
fixed from planning store".

## Decided in the operator's absence

Wave 1 runs: `story:archive-on-old-git` (verification on the integration branch, no implementor),
`story:discard-cache-survives-overlong-paths` (GitHub #44) and
`story:gc-dry-run-reports-as-it-assesses` (GitHub #45), one `aep:implementor` and one
`aep:adversary` each for the two implemented units, each in its own managed worktree.

`aep plan artifact waves --kind story --status active --format json` (aep 0.79.0) places exactly
these three ids in `"wave": 1`, prints no collision between any two of them, `"unassessed": []` and
`"cycles": []`. Check path: the verb. Scope: `archive-on-old-git` cited; the other two partly
inferred. No story left the wave.

Pre-flight: 79 G free on `/`; two concurrent builds; `sccache` not wired, builds are cold.

Commits this record authorises: one commit per implemented unit, the merges into
`fix/open-defects`, the closing store commit. The plan page states the push and the single
squash merge at the end of the plan. No tag, no version bump, no release.

## What the operator would have been asked

Whether to run wave 1 as proposed.
