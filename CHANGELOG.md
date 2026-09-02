# Changelog

## Unreleased

## 0.3.0 — 2026-09-02

- Prove recovery from exact refs currently advertised by configured remotes, including branches,
  tags, pull-request refs, and custom namespaces, while rejecting local tags and stale or fabricated
  remote-tracking refs and disabling local replacement/graft ancestry.
- Treat ignored files, operational Git locks, and paused merge/rebase/sequencer state as cleanup
  blockers, canonicalize disjoint policy roots, require exact linked-worktree membership, and bind
  create plans to immutable commits and policy-derived paths.
- Persist the final HEAD, atomically claim cleanup and relocation against lease acquisition,
  re-observe HEAD around durable proof-bearing removal intent, and recover interrupted removals.
- Extend reconciliation with interrupted-provisioning recovery and exact-id retirement of finished,
  clean external legacy worktrees behind separate confirmation, while retaining ordinary GC
  containment.
- Require exact reviewed ids for GC and reconciliation apply operations; publish CLI JSON protocol
  version 2 and reconciliation version 2 while retaining hook, configuration, and policy version 1.
- Keep agent skill and interface guidance deterministic and generator-owned.

## 0.2.0 — 2026-09-02

- Add reviewed legacy reconciliation that migrates adopted linked trees into the managed root and
  records safely missing worktrees without weakening cleanup containment.

## 0.1.0 — 2026-09-01

- Add the typed multi-crate worktree lifecycle library and CLI.
- Add XDG-state placement, SQLite ownership and lease records, recoverability-gated cleanup, and
  workspace audits.
- Add hook protocol version 1 and deterministic `worktree skill` generation.
