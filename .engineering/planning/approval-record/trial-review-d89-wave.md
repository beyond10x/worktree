---
format: aep.planning-md/3
id: approval-record:trial-review-d89-wave
kind: approval-record
status: accepted
title: 'Records D89: the trial-review stories are implemented in parallel units'
tags:
- wave-approval
relations:
- decides: story:a-nested-keep-file-keeps-its-directory
revision: 2
transitions:
- {from: "draft", to: "accepted", at: "2026-10-27T12:27:25Z", actor: "human:timo", revision: 2}
---
Decider: orchestrator, relayed to the trial-review coordinator.

Records decision D89, 2026-10-27, as the orchestrator sent it:

"D89: option A in parallel, nothing paused: one opus coordinator for the 15 trial-review stories (takt +
engineering-protocols + worktree + metaharness), fans out sonnet implementors, up to 4 at once, units by file overlap,
targeted tests (D85), merge at green into integration/takt / plan branches, reviews after; at most 2 takt trees
building at once, small repos free."

The owner's words it acts on, 2026-10-27: "So why can't you do this in parallel? Speed up your work, your subagents.
Be faster. It's unacceptable to wait so long for these small changes."

What it means for this store:

- The trial-review stories this record decides move to active and are implemented on a unit branch.
- The unit branch merges to main through one pull request once its stories are green and their adversary findings
  are fixed. The release follows through this repository's own process when the consumer needs the released version.
