# Changelog

## 0.7.1 — 2026-09-25

- `worktree doctor --check` exits non-zero and names `no active profile` when the configuration
  holds no workspace profile, since every `worktree create` would then lack a policy. The readiness
  decision moves into the library façade as `readiness_failures`. The success text line, the
  `doctor` JSON payload and every declared surface version are unchanged; the generated skill
  states the new exit behavior.

## 0.7.0 — 2026-09-24

- Every release publishes prebuilt binaries: `worktree-<version>-<target>.tar.gz` for
  `x86_64`/`aarch64` Linux and macOS, with `SHA256SUMS`, built and attached by the new
  `.github/workflows/release.yml` when the tag is pushed. `cargo install --git … --tag` still works.
- The `worktree` agent plugin moves to `beyond10x/agentplugins`, where every Beyond10x plugin lives;
  `plugins/worktree/`, the Codex marketplace file and the manifest-version test are removed here.
  Install it with `b10x` or as `worktree@b10x`; an earlier `worktree@worktree` install is migrated by
  `b10x setup`.
- `worktree skill` and its `--out` default are unchanged. No library, CLI, wire-protocol, schema or
  behavior change.

## 0.6.0 — 2026-09-24

- Ship the `worktree` agent plugin from `plugins/worktree/`, released with the binary at the same
  version. Claude Code installs it as `worktree@b10x` from the `beyond10x/agentplugins` catalog;
  Codex reads `.agents/plugins/marketplace.json` in this repository as `worktree@worktree`. It
  replaces the `workspace-hygiene` plugin from `beyond10x/agentplugins`, whose skill `worktree`
  becomes `worktree:worktree`.
- The generated skill moves from `.agents/skills/worktree/` to `plugins/worktree/skills/worktree/`;
  `task check` verifies it there, and a test refuses a plugin manifest whose version differs from
  the workspace package version.
- `worktree skill` output and its `--out` default are unchanged. No library, CLI, wire-protocol,
  schema or behavior change; every declared surface version is unchanged.

## 0.5.1 — 2026-09-24

- Run the shared source gate at Gates `db7edb1`, which downloads the Gates 0.1.7 release instead
  of 0.1.3.
- Point the AEP project file at protocol sources `733dbea`.
- Pin the documentation bundle action at Docs System `339b4b8` and add the read-only per-source
  documentation check on pull requests and `main` pushes.
- No library, CLI, wire-protocol, schema or behavior change; every declared surface version is
  unchanged.

## 0.5.0 — 2026-09-23

- Accept a second recovery proof kind, `patch-equivalent`, for work that was rebased or
  cherry-picked before it was merged. When no advertised ref contains the exact HEAD, one
  advertised ref must carry a whitespace-exact identical patch for every commit that no advertised
  ref holds, and Git's own cherry-pick equivalence must agree. A unique root, merge or empty commit
  defeats the proof. Ancestry proof is unchanged and tried first; GC, reconciliation and
  `inspect --refresh` all use both kinds, and the final pre-removal observation repeats them.
- **Wire change:** non-hook CLI JSON moves to protocol version 3, reconciliation JSON to version
  3 and inspection reports to `worktree.inspection/2`. Recovery proof gains `kind` (`ancestor` or
  `patch-equivalent`) and `equivalent_commits`. Stored proofs without these fields decode as
  ancestry proof. Hook protocol 1 and configuration schema 1 are unchanged.
- Add `GitPort::recovery_evidence`, returning `RecoveryEvidence`. Its default implementation
  reports ancestry from `recovery_refs`, so existing embedded ports keep compiling unchanged.
- Refresh the lockfile to the latest Rust 1.85-compatible releases: 19 packages, all patch or
  minor updates (clap 4.6.7, uuid 1.26.1, rustix 1.1.5, among them). Direct dependency
  requirements are unchanged.
- Add a reviewed path for a missing record whose recorded commit an operator has established is
  gone for good: `reconcile --apply --id <reviewed-id> --acknowledge-unrecoverable <commit>`
  tombstones it. The acknowledgement asserts one exact commit named by the immediately preceding
  dry-run, is refused while any local branch, tag, remote-tracking ref or remote advertisement
  still contains that commit, is refused when it matches no reviewed missing record, and is
  refused without `--apply`. It deletes nothing from disk or from Git — the tree is already
  absent — and records the tombstone as `reconcile-abandoned` with no recovery proof, because
  there is none.
- Say in the `missing-active-worktree` and `no-remote-recovery-proof` reconciliation refusals what
  to do next, naming the commit to publish and the exact command that abandons the record.
- Add `GitPort::containing_refs`, a local-only observation of every branch, tag and
  remote-tracking ref containing a commit, distinguishing a commit Git no longer holds at all from
  one that survives with nothing pointing at it.

## 0.4.1 — 2026-09-10

- Record the organization release-completion boundary in `AGENTS.md`: an ordinary source release
  completes on this repository's own tag, required checks, published release and required
  artifacts, while Atlas reconciliation and public documentation publication stay asynchronous
  and are reported as pending rather than waited on.
- No library, CLI, wire-protocol, schema or behavior change; every declared surface version is
  unchanged.

## 0.4.0 — 2026-09-07

- Add native `worktree inspect` with repository scope by default, explicit workspace expansion,
  bounded storage accounting, actual Git state, leases, retention blockers and optional fresh
  remote recovery evidence. Its versioned report does not authorize removal or infer work completion.
- Ignore a leftover `REBASE_HEAD` after a completed rebase while preserving real operation,
  dirty-file, lease, lock and remote recovery checks.
- Generate explicit lease acquisition, heartbeat, lease release before finish, bounded build
  storage, evidence retention and cleanup or handoff guidance.
- Authenticate the CI Task installer to avoid anonymous GitHub API rate limits.

## 0.3.4 — 2026-09-04

- File `story:worktree-diff` in the planning store this repository already had: one read-only verb
  showing the commits, per-file stat and patch a managed worktree adds over the revision it was
  created from, with a path filter and a JSON form. Filed, not implemented.
- Remove the nested duplicate store `.engineering/.engineering/` that 0.3.3 added by mistake;
  0.3.3's changelog line claiming to adopt the store was wrong — the store predates it.

## 0.3.3 — 2026-09-04

- Added a nested duplicate planning store by mistake (`.engineering/.engineering/`); corrected in
  0.3.4. No code change.

## 0.3.2 — 2026-09-02

- Say in the generated skill that `gc --repo` selects the activated workspace profile rather than
  the repository, so its dry-run assesses every record under that profile's `workspace_root` and
  an unreviewed apply would remove another repository's work.
- Say that `status` accepts no filter and reports every record in every profile, so its output must
  be read against each record's `repository_root`.

## 0.3.1 — 2026-09-02

- Retire finished external legacy trees that were stranded by a stale pre-0.3 relocation intent,
  but only when the exact source and HEAD still agree and the intended destination is absent.
- Retain that stale relocation alongside durable removal proof until successful non-forced
  removal, then clear both intents atomically, while refusing ambiguous or partially moved state.
- Teach the generated Worktree skill how to handle this reviewed recovery path without converting
  cross-device migration refusals by hand.

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
