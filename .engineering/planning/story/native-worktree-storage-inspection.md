---
format: aep.planning-md/1
id: story:native-worktree-storage-inspection
kind: story
status: implemented
title: Inspect storage and retention blockers through the worktree facade
relations:
- informed_by: story:scope-flag-for-reporting-subcommands
- informed_by: story:rebase-head-is-not-a-lock
revision: 4
---
## Outcome

An operator investigating disk pressure can run `worktree inspect --repo <path>` to see current worktree state, occupied storage and why retention continues. Repository scope is the default; `--workspace` explicitly expands to its activated profile, and repeatable `--id` narrows that scope. Inspection includes active records that ordinary GC omits until expiry.

## Evidence and contract

The 7 September disk audit found substantial duplicate compiler output, registry HEADs behind actual HEADs, clean published work still recorded active, almost no maintained leases, and ignored raw data inside a completed-story tree. `crates/worktree/src/lib.rs` separates finish and GC and filters young active trees from GC; `crates/worktree-git/src/lib.rs` treats ignored state as dirty. Those are inspection inputs, not permission to discard files.

The report is a new separately named `worktree.inspection/1` surface inside the existing CLI envelope. Existing JSON, hook, configuration and registry formats retain their bytes and meaning. New domain observation: `.engineering/specs/worktree-inspection/system.yaml`, validated with the installed ESS CLI.

## Scope and acceptance

- Public library orchestration owns selection and assessment; CLI renders results.
- Show record purpose/owner/lifecycle/recorded HEAD alongside actual HEAD and branch, separate tracked/untracked/ignored counts, live leases and observed age. No lease is not proof that an owner has abandoned work.
- Count logical and allocated storage where supported, deduplicate hard links, do not follow symlinks, and bound traversal. Partial observations are explicit and never reported as complete zero usage.
- Include missing/unreadable paths and Git failures as per-record findings; one bad tree must not hide the rest.
- Default inspection performs no network calls or lifecycle writes. `--refresh` explicitly obtains current advertised remote-ref recovery facts, may fetch missing Git objects, and detects HEAD changes during the proof.
- Show finish/expiry blockers and preserve current GC/removal gates. Inspection does not authorize deletion, infer completion from age or modify story/issue states.
- Task association remains explicitly unavailable in the current registry: free-text purpose is shown, but changed planning files and coincident tokens are not promoted to ownership. Consumer integrations must eventually supply explicit references and observed status through typed adapters above this portable library.
- `task check` plus process-level tests cover repository versus workspace scope, ignored evidence, partial size, symlink/hard-link accounting, missing paths, fresh recovery and unchanged lifecycle state.

## Follow-through required in consumers

This command makes retention visible; it does not by itself prevent accumulation. Harness/plugin owners must wire session leases and an explicit end-of-work disposition; wave owners must record publication/handoff and finish/GC outcomes. Build/scratch owners must declare disposable resources separately from retained evidence, with bounded budgets and expiry. Do not archive entire Cargo targets as evidence. These cross-repository changes require their own governed implementation work and are not represented as delivered by this story.

## Recovery policy

Exact currently advertised remote branches or tags protect detached/local-only commits after checkout retirement. A local branch or verified bundle could support a separately designed recovery mode; neither is admitted by current removal policy. Build cleanup must be independently available so unpublished source does not force retention of disposable gigabytes.
