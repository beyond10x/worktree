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
- Every removal requires an exact linked-worktree member whose HEAD remains stable across the final
  proof and intent observations, no tracked, untracked, or ignored state, no live lease or Git
  operational/worktree lock or in-progress Git operation, and fresh proof from exact refs currently
  advertised by a configured remote.
- Ordinary GC requires canonical containment below the configured worktree root. Only exact-id
  reconciliation with separate external-retirement confirmation may retire a finished external
  legacy tree after the same removal gates pass.
- Never clear stale relocation state by hand. A finished external legacy tree may supersede a
  pre-0.3 relocation intent only when the exact source and all recorded HEADs agree and the
  destination is absent from both Git and the filesystem; ambiguous state remains a refusal.
- Offline or ambiguous recovery evidence is a refusal, never permission to delete.
- Local replacement refs, graft files, and inherited graft configuration must never influence
  remote recovery proof.
- Create plans use immutable commits and canonical policy-derived paths; plans and exact repository
  membership are revalidated immediately before mutation.
- Apply operations for GC and reconciliation require exact reviewed worktree ids. Cleanup and
  relocation lifecycle claims, final HEAD updates, and lease exclusions are atomic, and
  proof-bearing removal intent is durable.
- CLI JSON protocol version 2, reconciliation version 2, hook protocol version 1, and
  configuration/workspace-policy version 1 are immutable after release. Cut a new surface version
  for a wire change.
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

This repository owns the public source and presentation allowlist in `b10x.docs.yaml`. The generated credential-free `.github/workflows/b10x-docs-bundle.yml` passively packages only those declared files for the exact successful `main` commit; it must never run repository code. Atlas selects the latest successful bundle with every other catalog source, and Website plus Docs System own rendering, shared components, search, and feeds. Do not add a standalone docs deployer or put App credentials in this public repository. If Atlas catalogs a former Pages workflow, that file remains repository-owned validation: preserve its bespoke checks while keeping exact read-only permissions, an unconditional pull-request trigger, and no deployment primitives. Project Pages at `/worktree/` is only the generated stable redirect façade in `.github/workflows/b10x-docs-pages.yml`; content-only publication never rebuilds it.

From the complete organization workspace, verify the contract with a clean Atlas checkout at the current remote `main`. Set `B10X_ATLAS_CHECKOUT` to a managed Atlas worktree when the primary checkout is dirty or stale; never infer command availability from the primary alone.

```bash
atlas_checkout="${B10X_ATLAS_CHECKOUT:-atlas}"
atlas_head="$(git -C "$atlas_checkout" rev-parse HEAD)"
atlas_main="$(git -C "$atlas_checkout" ls-remote origin refs/heads/main | awk '{print $1}')"
test -z "$(git -C "$atlas_checkout" status --porcelain)"
test "$atlas_head" = "$atlas_main"
cargo run --manifest-path "$atlas_checkout/Cargo.toml" --locked -q -- \
  --store "$atlas_checkout/catalog/store" docs reconcile --workspace . --check
```

Keep internal plans, stories, ADRs, decisions, worklogs, security material, and research out of the public allowlist unless a repository authority explicitly declares them public.
<!-- b10x-docs-operations:end -->
