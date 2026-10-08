---
format: aep.planning-md/3
id: story:tree-reference-resolves-everywhere
kind: story
status: implemented
title: One tree reference works in finish, gc and reconcile
summary: finish, discard-cache, archive and the --id of gc and reconcile accept an id, a tree path or a unique tree directory name; finish prints the id
scope:
- confidence: cited
  path: crates/worktree-cli/src/main.rs
- confidence: cited
  path: crates/worktree-cli/tests/tree_reference.rs
- confidence: cited
  path: crates/worktree/src/lib.rs
revision: 7
transitions:
- {from: "draft", to: "proposed", at: "2026-10-08T09:10:11Z", actor: "human:timo", revision: 4}
- {from: "proposed", to: "active", at: "2026-10-08T09:10:11Z", actor: "human:timo", revision: 5}
- {from: "active", to: "implemented", at: "2026-10-08T09:26:39Z", actor: "human:timo", revision: 7, decided_on: {"recorded":{"test_result":1,"review_outcome":2}}}
---
## Outcome

Every command that takes one tree (`finish`, `discard-cache`, `archive`) and every `--id` of `gc`
and `reconcile` accept the same tree reference: a registered worktree id, a path to a registered
tree, or the directory name of exactly one registered tree. `finish` prints the id it finished.

## Why

`finish` takes a path and `gc --apply` takes an id. On 2026-10-08, agents in 4 repositories hit 9
`worktree-not-found` / `unknown-worktree-id` refusals passing a tree name or path to `gc`, and
`invalid-worktree-id` for names with dots (a tree directory such as `hard-defects-0.7.0` is not a
valid id). `finish` prints only `finished <path>`, so its output cannot be pasted into `gc --id`.

## Specification

`worktree.selection.ReferenceForm` and `worktree.selection.TreeSelection` in
`.engineering/specs/worktree-inspection/domains/selection.yaml`; validated with `ess` 0.56.0.

## Contract

- A reference resolves as `Id` when it is a registered id; as `Path` when it names a directory
  whose canonical path equals a registered record's path; as `DirectoryName` when it contains no
  `/` and is the last component of exactly one in-scope record's path.
- Two forms naming different records refuse as `ambiguous-worktree-reference` and list both ids.
  Two records sharing a directory name refuse the same way. Nothing matching keeps the released code: the tree argument of `finish`,
  `discard-cache` and `archive` refuses as `worktree-not-found`; `--id` refuses as
  `unknown-worktree-id` for a valid id and `invalid-worktree-id` otherwise. A directory in the
  working directory that is not a registered tree never shadows a registered id.
- The worktree id grammar is unchanged (no dots): a registry with a dotted id cannot be loaded by
  0.11.0-0.12.1 (`crates/worktree-state/src/lib.rs:491`). The `invalid-worktree-id` refusal of
  `create --id` says to use hyphens.
- `gc --apply` keeps its exact-review rule: the resolved ids are the reviewed ids, and the output
  names each id.
- `finish` human output prints `finished <id> <path>`; the JSON payload carries the id (already in
  the evidence record).
- Resolution lives in the library (`crates/worktree/src/lib.rs`); the CLI calls it.

## Acceptance

- `gc --dry-run --id <tree path>` and `--id <directory name>` assess the same record as `--id <id>`.
- `gc --apply --id <path printed by finish>` removes the finished tree.
- `finish <id>` run outside the tree finishes it.
- A dotted directory name resolves; `create --id a.b` still refuses.
- Ambiguous and unknown references refuse with the codes above.
- The generated skill (`worktree skill`) states that one reference works everywhere.
