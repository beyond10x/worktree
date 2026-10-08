---
format: aep.planning-md/3
id: review-result:adversary-tree-reference-pass-1
kind: review-result
status: active
title: Adversary, tree reference, pass 1
summary: '2 findings on 6f9f5ae, both introduced refusals: a plain directory shadowed a registered id, and gc --id lost the released unknown-worktree-id code'
relations:
- reviews: story:tree-reference-resolves-everywhere
revision: 1
---
needs-revision

Adversary pass 1 on 6f9f5ae (story:tree-reference-resolves-everywhere). 4 cases added to
`crates/worktree-cli/tests/tree_reference.rs`; 2 red, both introduced, both refusals (no case
acted on a wrong tree).

| # | finding | outcome |
|---|---|---|
| 1 | `finish docs` from a directory holding a plain `docs/` refused `unmanaged-worktree` instead of finishing the tree whose id is `docs` | fixed in e0cc4ac: the reference is resolved before an unregistered path passes through |
| 2 | `gc`/`reconcile --id` with an unregistered valid id refused `unknown-worktree-reference` instead of the released `unknown-worktree-id` | fixed in e0cc4ac: released codes kept; the spec comment and story contract say so (77e23d7) |

Held: gc/reconcile apply by id, path or name never reached a tree outside the policy's workspace;
`finish <id>` from outside keeps the live-lease refusal; JSON payload shapes unchanged.

Noted, not changed: `finish`, `discard-cache` and `archive` resolve a bare reference across the
whole registry, so a directory name can name another repository's tree. Ids are registry-unique;
1 of 6274 live records had a directory name different from its id.
