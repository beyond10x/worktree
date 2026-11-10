# Wave plan: open defects and GitHub issues, November 2026

Epic: `epic:open-defects-2026-11`. Integration branch: `fix/open-defects`, forked from `main` at
`23b9c91`. One pull request holds every wave. No release is cut.

The run is non-interactive (a coordinator sub-agent held by a Stop hook; no operator turn). Each
bypassed stop is an `approval-record` tagged `non-interactive`; they are listed at the end.

Skill: `aep:implementing` 0.26.0, wave mode. Agents: `aep:story-scoper`, `aep:implementor`,
`aep:adversary`. At most 4 builds at once.

## The plan

Derived once with `aep plan artifact waves --kind story --status active --format json` (aep 0.79.0)
after the scopers' typed scope was written. The verb's placement, verbatim by id:

| wave | stories | objective |
|---|---|---|
| 1 | `story:archive-on-old-git`, `story:discard-cache-survives-overlong-paths`, `story:gc-dry-run-reports-as-it-assesses` | GitHub #44 and #45; prove the old-Git fix that already shipped |
| 2 | `story:rebase-head-is-not-a-lock` | the refusal names the marker; doctor notes a stale `REBASE_HEAD` |
| 3 | `story:skill-prints-without-out` | prove the fix that already shipped |
| 4 | `story:strip-dry-run-is-fast` | the strip dry run shares per-repository work and reports progress |

Collisions the verb printed (13): `archive-on-old-git` with `strip-dry-run-is-fast` on
`crates/worktree-git/src/archive.rs` (cited); `gc-dry-run-reports-as-it-assesses`,
`rebase-head-is-not-a-lock`, `skill-prints-without-out` and `strip-dry-run-is-fast` pairwise on
`crates/worktree-cli/src/main.rs` (cited); the first, second and fourth of those pairwise on
`crates/worktree/src/lib.rs` (cited) and `crates/worktree-git/src/lib.rs` (inferred). Unassessed:
none. Cycles: none.

Scope confidence: `archive-on-old-git` and `skill-prints-without-out` cited; the other four rest
partly on inferred entries.

`story:archive-on-old-git` and `story:skill-prints-without-out` are verification units: the code
is believed to be on `main` already, so no implementor is dispatched; the coordinator runs the
story's own tests on the integration branch and records the output.

Shared files (`CHANGELOG.md`, `AGENTS.md`, `README.md`, `docs/architecture.md`, everything under
`.engineering/`) are the coordinator's. A unit reports the lines it needs there.

## Commits this plan authorises

Per wave: one commit per implemented unit, the merges into `fix/open-defects`, one closing store
commit. At the end: one push of `fix/open-defects` and one squash merge of its pull request into
`main` through `b10x-bot`. No tag, no version bump, no release.

## Pre-flight (2026-11-09)

- `main` checkout: untouched, 5 commits behind `origin/main`; every tree forks from `origin/main`.
- `worktree repo list`: the primary and the integration tree only.
- Free disk on `/`: 79 G (91 % used). One cold debug build of the test binaries: measured below.
- `sccache` present, `RUSTC_WRAPPER` unset: builds are cold per tree.
- Model budget: not stated by anybody; N defaults to 4 and no wave exceeds 2 builds.

## Units

Trees are managed (`worktree create`), ids below; paths are `<worktree root>/b10x/worktree/<id>`.
Build directory: `target/` inside each tree. Scratch: `<session scratch>/worktree/units/<unit>/`.

| unit | tree id | branch | stage | head |
|---|---|---|---|---|
| integration | `open-defects-integration` | `fix/open-defects` | open | |
| `archive-on-old-git` | (integration) | | wave 1, verification | |
| `discard-cache-survives-overlong-paths` | `open-defects-u44` | `impl/discard-cache-survives-overlong-paths` | wave 1, planned | |
| `gc-dry-run-reports-as-it-assesses` | `open-defects-u45` | `impl/gc-dry-run-reports-as-it-assesses` | wave 1, planned | |
| `rebase-head-is-not-a-lock` | `open-defects-rebase` | `impl/rebase-head-is-not-a-lock` | wave 2, planned | |
| `skill-prints-without-out` | (integration) | | wave 3, verification | |
| `strip-dry-run-is-fast` | `open-defects-strip` | `impl/strip-dry-run-is-fast` | wave 4, planned | |

## Waves

### Wave 1 of 4

Proposed and run under `approval-record:wave-1-open-defects-2026-11-09`.

Stopped 2026-11-09 by operator correction (the goal applies to another repository): no pull
request, no merge, no issue closed. `story:archive-on-old-git` has its `test_result` recorded and
stays `active`. Units #44 and #45 are committed on their `impl/` branches, not yet attacked by an
adversary and not merged. Waves 2 to 4 were not started.

### Wave 2 of 4

### Wave 3 of 4

### Wave 4 of 4

## Backlog filed from findings

None yet.

## Approval records

- `approval-record:open-defects-acceptance-2026-11-09` (acceptance of the stories)
- `approval-record:wave-1-open-defects-2026-11-09`
