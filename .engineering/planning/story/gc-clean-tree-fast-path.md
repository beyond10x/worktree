---
format: aep.planning-md/3
id: story:gc-clean-tree-fast-path
kind: story
status: draft
title: gc accepts a clean tree whose HEAD a remote advertises without fetching or scanning every ref
summary: A clean tree already on the remote is cleared by comparing commit ids, not by a fetch and a contains scan
revision: 2
---
## Outcome

`worktree gc` clears a clean tree whose HEAD is already on a remote in seconds, however large the repository. When HEAD equals a commit id the remote advertises, or is an ancestor of a ref already present locally, the recovery proof is accepted by that comparison alone, with no fetch and no scan of every ref.

## Context

Reported 2026-11-02 by a release-review session using worktree CLI 0.19.0, which relayed the operator's words: "It could just compare the commit hashes". We cannot verify that quote ourselves.

Observed, not yet measured per step:

- `worktree gc --id <tree> --apply` took 1 to 3 minutes or more per clean tree whose HEAD was already on the remote (branch pushed, merge request merged), on a large application monorepo and on a chart repository. On small repositories it took seconds.
- One gc process had read 1.7 GB (rchar 1700485956, wchar 15 MB) and sat blocked in poll_schedule_timeout, with open file descriptors only on registry.sqlite3 and a temporary file.
- Which step takes the time is not known.

Code path read at 41630ff, `assess_cleanup` in `crates/worktree/src/lib.rs`: `exact_worktree_snapshot` (`git status --ignored=matching`), `hidden_state`, then `recovery_for_record`, which reaches `crates/worktree-git/src/lib.rs`: `ls-remote --quiet --refs` (line 184), `fetch --filter=blob:none --stdin` of the advertised refs (line 243 at that commit), and `for-each-ref --contains <head>` (line 976). Hypothesis, unverified: the fetch plus the `--contains` scan over every ref dominate the time.

The recovery proof stays what AGENTS.md Invariants says it is: fresh proof from exact refs currently advertised by a configured remote. This story changes how cheaply an exact match is recognised, not what proves recovery.

Related: `story:strip-dry-run-is-fast` is a different command (`prune-archives --strip-build-output`), not this path.

## Acceptance

- Measurement first: per step wall time of `gc --id <tree> --dry-run` and `--apply` on a clean tree whose HEAD is on the remote, in a large repository, split into status snapshot, hidden-state check, `ls-remote`, `fetch`, and `for-each-ref --contains`, recorded in the story or its review result before any change.
- A test over a clean tree whose HEAD equals a commit id that `ls-remote` advertises asserts the git commands gc invoked include no `git fetch` and no `git for-each-ref --contains`, and that gc reports the tree eligible.
- The same assertion for a clean tree whose HEAD is an ancestor, by `merge-base --is-ancestor HEAD <ref>`, of a ref already present locally.
- The recovery proof records which check satisfied it (advertised-equal, local-ancestor, or the existing full proof), visible in the gc result.
- A tree whose HEAD matches no advertised commit and is no ancestor of a local ref still takes the existing path, and every existing refusal test still passes unchanged: dirty tree, hidden state, patch-equivalent, archive proof.
- The fast path accepts only a tree gc already found clean; it weakens no invariant in AGENTS.md: HEAD stays stable across the final proof and intent observations, and removal stays non-forced.

## Out of Scope

- Any change to the archive proof, the patch-equivalence proof, or removal itself.
- Caching remote advertisements across gc runs.
- Parallel assessment of several trees.
- A stale local ref as proof: a local-ancestor match must be against a ref the same run freshly advertised, or the story must say how freshness is shown (see Ambiguities).

## Ambiguities

- Whether a local ref that is not currently advertised may count for the local-ancestor check. The AGENTS.md invariant requires refs currently advertised by a configured remote, so this story assumes only advertised refs count and the local check applies to objects already present for an advertised id. `inferable`, from the Invariants section of AGENTS.md ("fresh proof from exact refs currently advertised by a configured remote"). Confirm during measurement.
- Which step dominates the time. `inferable` only by measurement; it decides whether skipping the fetch alone is enough.
