# AGENTS.md — worktree

## Serves

- **O2 — decisions as data, with evidence.** Worktree placement, ownership, recoverability and
  cleanup are typed decisions with recorded proof.
- **O6 — self-improvement, built into all of it.** Agent work leaves bounded, inspectable state and
  a deterministic cleanup path instead of accumulating invisible machine debris.

## Boundary

This repository owns safe Git worktree lifecycle as a reusable Rust library and CLI. It knows no
Atlas, agent harness, plugin marketplace or organization repository inventory. Consumers supply
profiles and adapters from above.

The `b10x-worktree-domain` crate performs no I/O. `b10x-worktree` owns the application ports and
orchestration. Concrete Git and SQLite behavior stays in their adapter crates. The CLI contains no
independent policy: every decision must come from the public façade.

## Invariants

- Never invoke a command through a shell. Git arguments are discrete argv values.
- Never remove a worktree with `--force`.
- A cleanup requires a clean tree, no live lease or Git lock, fresh remote evidence, and canonical
  containment below the configured worktree root.
- Offline or ambiguous recovery evidence is a refusal, never permission to delete.
- Plans are revalidated immediately before mutation.
- JSON and hook protocol version 1 are immutable after release. Cut a new protocol version for a
  wire change.
- Generated skill content comes from `worktree skill`; do not edit it by hand.
- A public API belongs in a library crate. The binary is an adapter, not the product boundary.

## Gate

```console
task check
```

Anything executable in this repository is Rust. Releases use bare SemVer tags from `main`, after
`CHANGELOG.md`, every workspace package version and `Cargo.lock` agree.

