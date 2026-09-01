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

<!-- b10x-docs-operations:start -->
## Public documentation operations

This repository owns the public source and presentation allowlist in `b10x.docs.yaml`; the unified [beyond10x Website](https://beyond10x.github.io/docs/worktree/) passively collects those declared files from the exact commit in `website/sources.lock.json`. Atlas owns discovery grouping/order; Website and Docs System own rendering, shared components, search, and feeds. Do not add a standalone docs deployer or put App credentials in this public repository. If Atlas catalogs a former Pages workflow, that file remains repository-owned validation: preserve its bespoke checks while keeping exact read-only permissions, an unconditional pull-request trigger, and no deployment primitives. Project Pages at `/worktree/` is only the generated redirect façade in `.github/workflows/b10x-docs-pages.yml`.

From a complete organization workspace, run `cargo run --manifest-path atlas/Cargo.toml -- docs reconcile --workspace . --check` to verify the contract. Keep internal plans, stories, ADRs, decisions, worklogs, security material, and research out of the public allowlist unless a repository authority explicitly declares them public.
<!-- b10x-docs-operations:end -->
