---
format: aep.planning-md/3
id: story:archived-tree-can-be-retired
kind: story
status: implemented
title: A tree whose commits are archived locally can be retired
revision: 4
transitions:
- {from: "draft", to: "proposed", at: "2026-09-26T21:34:24Z", actor: "human:timo", revision: 2, imported: true}
- {from: "proposed", to: "active", at: "2026-09-26T21:34:25Z", actor: "human:timo", revision: 3, imported: true}
- {from: "active", to: "implemented", at: "2026-09-26T21:34:26Z", actor: "human:timo", revision: 4, decided_on: {"recorded":{"test_result":1}}, imported: true}
---
# A tree whose commits are archived locally can be retired

## Outcome

`worktree gc` accepts a verified local archive as recovery proof for a tree whose commits are on no remote ref: a Git bundle that contains every commit unique to the tree, verified against the repository, plus the tree's uncommitted state, recorded against the tree id. With that proof, a finished tree is removed like one whose commits are on a remote.

## Why

On 2026-09-26, 96 of the b10x managed trees were retained with `no-remote-recovery-proof`: 57 ESS, 10 Atlas, 7 Connectors, 6 llm, 6 AEP and 10 across 8 more repositories. Most are idle for days to weeks. Their commits were mostly squash-merged or abandoned, so no advertised ref carries a patch-identical commit. Pushing them to hidden refs on public repositories would publish unreviewed commits, so they stay on disk.

## Acceptance

- An archive command writes the bundle and a manifest (tree id, HEAD, unique commits, bundle digest) to a configured archive root and verifies the bundle with `git bundle verify`.
- `gc --dry-run` reports such a tree as eligible, naming the archive; `gc --apply --id` removes it and keeps the archive.
- A bundle that does not contain every unique commit, or whose digest changed, is refused by name.
- Nothing changes for trees without an archive.
