# Changelog

## Unreleased

- `worktree archive` (and `finish --archive`, and `sweep`) leaves cargo's own build layout out of
  an archive. Below a target recognised by `discard-cache`'s structure (also when a tracked file
  below it hides it from `discard-cache`), an untracked file whose first component below the
  target is `debug`, `release`, `.rustc_info.json`, `tmp`, a Cargo-signed `CACHEDIR.TAG` or a
  profile directory (holding `.fingerprint/`; also `<target>/<triple>/<profile>/`) is neither in
  `dirty.patch` nor in the fingerprint. Tracked files below the target stay archived. Everything else below the target, such as
  `target/ess-conformance/`, stays archived. Such an archive is the new manifest format
  `worktree.archive/3`, which records per target the files and bytes left out as `build_output`;
  the command prints `left out <files> file(s), <bytes> bytes of cargo build output under
  <target>`. An archive that left nothing out is written byte for byte as 0.14.0 writes format 1
  or 2, and both stay readable.
- Removal through a format 3 archive (`gc --apply`) deletes each left-out file as cache only after
  re-observing it, immediately before deletion, as build layout below the same recognised target.
  A left-out path that is no longer build layout, or layout below a target the archive did not
  record, refuses as `archive-stale`.
- `worktree prune-archives --strip-build-output [--repo] [--scope repo|profile] [--id]...
  [--dry-run|--apply]` removes build layout from archives written before. A dry-run, the default,
  streams each `dirty.patch` and lists the bytes and sections it would strip, what stays, and the
  prune verdict afterwards. Only whole sections that purely add an untracked file (`new file
  mode`, a C-unquoted path no other section names and HEAD's tree does not track) whose path is
  build layout below a directory the patch itself recognises as a cargo target (a Cargo-signed
  `CACHEDIR.TAG` or a profile's `.fingerprint/`; `tmp/` only beside a profile) are stripped; both
  halves of a type change, other tracked changes, undecodable paths and nested images stay. `--apply --id` verifies the new patch over
  HEAD in a scratch index, recomputes `worktree_tree`, replaces the patch (or deletes it) and writes
  the format 3 manifest last, then prints the bytes freed. It refuses an invalid manifest,
  unrecorded content, a present tree, a gone repository or an unusable patch and leaves that
  archive unchanged; it never deletes an archive. On 2026-10-10, 19.18 GiB of one machine's
  31.52 GiB of `UncommittedState` archives was build output under `target/`.

## 0.14.0 — 2026-10-08

- `worktree prune-archives` removes archives a remote fully holds. A dry-run, the default, lists
  each archive directory with its bytes and verdict, then the bytes removable and refused.
  `--apply --id <directory>` deletes an archive only when HEAD and every unique commit it records
  are ancestors of a ref a configured remote freshly advertises, it holds no `dirty.patch` and no
  nested repository image, its tree is gone, and its directory holds only the files its manifest
  names. Every other archive is refused with its reason (`CommitsNotOnRemote`, `UncommittedState`,
  `NestedRepositories`, `TreeStillPresent`, `RemoteProofUnavailable`, `UnrecordedContent`,
  `InvalidManifest`) and kept; no flag forces a removal. Selection follows `gc`: `--repo`,
  `--scope repo|profile`, `--id`.
- An archive manifest that names a bundle or patch file other than `commits.bundle` and
  `dirty.patch` is refused as `archive-invalid`; no release writes another name.
- The GitHub Release body is the version's section of this changelog, not the tag message.

## 0.13.0 — 2026-10-08

- One tree reference works everywhere: the tree argument of `finish`, `discard-cache` and
  `archive`, and every `--id` of `gc` and `reconcile`, accept a registered id, a path to a
  registered tree, or the directory name of exactly one registered tree (so a dotted directory
  such as `hard-defects-0.7.0` resolves). Two forms naming different records, or two records
  sharing a directory name, refuse as `ambiguous-worktree-reference`. An unmatched value keeps the
  released code: `worktree-not-found` for the tree argument, `unknown-worktree-id` or
  `invalid-worktree-id` for `--id`. The id grammar is unchanged; `create --id` with a dot says to
  use hyphens, because 0.12.1 and earlier cannot load a registry holding a dotted id.
- `finish` prints `finished <id> <path>`, so its output is a valid `gc --id` value.
- `gc` without `--id` assesses only the records of the repository `--repo` resolves to. The new
  `--scope profile` keeps the previous profile-wide selection. In this repository's checkout,
  `gc --dry-run` took 36.7 s for 9 other repositories' trees with 0.11.0 and takes 0.04 s with
  the default scope. With `--id`, the named records are assessed whatever the scope.

## 0.12.1 — 2026-10-08

- `discard-cache` and `finish --discard-cache` recognise a Cargo target that has no
  `CACHEDIR.TAG`. Cargo writes the tag only when it creates the directory itself, so a `target/`
  made before the first build (`mkdir -p target/<dir>`) has none and was retained whole. Such a
  real directory now counts as tagged when it is named `target`, a tracked `Cargo.toml` sits
  beside it, and a direct child other than `tmp/` holds both `.fingerprint/` and `deps/`; its
  profiles go as `cargo-profile`, its `tmp/` as `cargo-target-tmp`, and every other child is kept.
  A `CACHEDIR.TAG` with a bad signature inside it is kept. No new cache kind.
- Tests: the `cache_discard` fixtures run Git with `maintenance.auto=false`, so a detached
  `git maintenance` cannot write into a nested fixture repository after a commit returns.

## 0.12.0 — 2026-10-07

- `worktree archive` (and `finish --archive`, `sweep`) images each nested Git repository in a
  tree's files, such as a test fixture kept in an ignored evidence directory, as
  `nested-<n>.tar`: the root and every entry below it, `.git` included, with permission bits,
  modification times and symlink targets. `tar -xpf <archive>/nested-<n>.tar -C <tree>` restores
  it with its committed, staged, unstaged, untracked and stashed state. Before, such a tree was
  refused as `archive-unsupported-entry` and could never be retired
  (https://github.com/beyond10x/worktree/issues/23).
- Each image records a fingerprint, the SHA-256 of a canonical listing of the root and every entry
  below it. The image read back and the directory before and after writing must all give it;
  removal re-verifies image digests, the set of nested roots and each fingerprint, and `gc --apply`
  deletes an imaged root bottom-up, re-checking each entry against the image immediately before
  deleting it.
- Still refused as `archive-unsupported-entry`: a nested `.git` that is a file or a symlink
  (submodules, linked worktrees), `commondir`, non-empty `objects/info/alternates`, `worktrees/`
  entries or a symlink inside a nested `.git`, and a root at or below which HEAD or the index
  tracks anything.
- An archive that images a nested repository is `worktree.archive/2`, adding
  `nested_repositories`; every other archive is still `worktree.archive/1` byte for byte. A 0.11
  reader refuses `/2` as `archive-invalid`.
- `gc --apply` on an archived tree makes a directory it must empty owner-writable before deleting
  its verified entries, as Git's own removal already did. Before, a read-only ignored directory
  made the discard stop with `archive-discard-failed` after deleting part of the tree.
- `discard-cache` and `finish --discard-cache` delete a Cargo target's real `tmp/`
  (`CARGO_TARGET_TMPDIR`) whatever it holds when the tagged target holds a profile: a target of
  profiles and test scratch goes whole as `cargo-target`, and beside retained content `tmp/` is
  listed as `cargo-target-tmp`. A tree whose tests wrote there no longer needs `--archive`.
- Library: `GitPort::hidden_state` takes the archive a caller verified in the same flow;
  `ArchiveManifest` gains `nested_repositories`; `CacheKind` gains `CargoTargetTmp`.
- The planning store moved to `aep.project/5`.

## 0.11.0 — 2026-10-07

- `worktree skill` without `--out` prints the generated skill (`SKILL.md`) to standard output and
  writes no file. Before, it wrote `.agents/skills/worktree/` below the working directory, so an
  agent that ran it to read the skill left an untracked copy in whichever checkout it stood in.
  `worktree skill --out <dir>` writes `SKILL.md` and `agents/openai.yaml` as before; `--check` and
  `--force` now require `--out`.
- `worktree --json skill` without `--out` is refused as `operation-failed`; with `--out` its JSON
  protocol 4 envelope is unchanged.
- The guidance block `worktree activate --install-agent-guidance` writes now says that outside the
  plugin `worktree skill` prints the skill to standard output and writes no file. Installed copies
  change only when the command runs again.

## 0.10.0 — 2026-10-06

- Add `worktree sweep [--repo <path> | --all-profiles] [--dry-run] [--idle-days N]
  [--max-archive-mib N]`. For every active or finished tree without a live lease and without
  activity for `--idle-days` (default 1), it runs `discard-cache`. For a tree past the profile's
  expiry, or finished, it then writes an archive of whatever no advertised ref recovers unless one
  already matches; above `--max-archive-mib` (default 1024) of retained ignored content it refuses
  as `archive-too-large` and writes nothing. Each record's refusal is reported in its item and the
  sweep goes on. It never changes lifecycle, never removes a tree and never applies GC: a swept
  tree shows as eligible in `worktree gc --dry-run`, and removal stays an exact-id apply.
- Idle time counts from the later of the registry's recorded activity and the tree's own Git
  index, HEAD and HEAD log, so a tree an agent worked in without a lease is not treated as idle.
- The README shows a systemd user timer that runs `worktree sweep --all-profiles` daily, and the
  generated skill tells agents what the sweep does.
- `CacheClassification` and `CacheDiscard` gain `retained_bytes` (default 0 when absent).
  `GitPort` gains `last_activity`, observing nothing by default; `WorktreeManager` gains `sweep`
  with `SweepOptions`; the domain gains `SweepItem`. JSON protocol 4 unchanged: `sweep` is a new
  command with its own `items` payload, and `discard-cache` adds the `retained_bytes` field.

## 0.9.0 — 2026-10-06

- Add `worktree discard-cache [<tree>] [--dry-run]`. It deletes the ignored directories it
  recognises as build cache by structure, never by name: a Cargo profile (holding `.fingerprint/`)
  inside a target carrying a valid `CACHEDIR.TAG`, directly or below a target-triple or nested
  tagged target; `node_modules` at or below a tracked npm, Yarn, pnpm or Bun lockfile; a virtual
  environment (`pyvenv.cfg`) beside a tracked Python manifest; and a tagged `.pytest_cache`,
  `.mypy_cache` or `.ruff_cache`. A tagged target holding only profiles, Cargo metadata and empty
  directories goes whole. Every other ignored entry is kept and listed, including records written
  inside `target/`. No symbolic link is followed. `--dry-run` classifies and deletes nothing.
- Applying is refused as `live-session` for a live lease, for a Git-locked tree, and as
  `worktree-in-use` while a process other than the caller and its ancestors has its working
  directory, executable or an open file in the tree (read from `/proc` on Linux; elsewhere the
  report sets `processes_observed: false` and only the lease guards the tree).
- `worktree finish` gains `--discard-cache`, which discards first, and `--archive`, which archives
  whatever the tree still holds that no advertised ref recovers (a tree still differing from HEAD,
  or HEAD adding commits no advertised ref holds) unless an existing archive already matches. After
  a discard without `--archive`, a tree that still differs from HEAD is refused as
  `worktree-dirty`, naming the kept entries. `finish` without the new flags is unchanged.
- Before this, `finish` and `gc` refused any tree holding ignored build output, and the guidance
  told agents to delete only directories they could prove they owned. On 2026-10-06, 105 active
  trees across four profiles (49.0 GB) differed from HEAD only by ignored files, and a by-name prune
  of every ignored `target/` deleted agent records that committed evidence cites below `target/`.
- The generated skill and the installed agent guidance end work with `worktree finish
  --discard-cache --archive <tree>` and tell agents to keep records out of ignored build
  directories.
- `GitPort` gains `discard_cache`, refusing by default as `cache-discard-unsupported`;
  `WorktreeManager` gains `discard_cache` and `finish_with` with `FinishOptions` and
  `FinishEvidence`; the domain gains `CacheDiscard`, `CacheClassification`, `DiscardedCache`,
  `CacheKind` and the recognition constants. JSON protocol 4 is unchanged: `discard-cache` is a new
  command with its own `cache` payload, and `finish` adds the optional `cache` and `archive` keys
  only when the new flags are given.

## 0.8.2 — 2026-09-27

- `worktree reconcile` retires a record whose repository was deleted. Before this, every such
  record failed as `git-command-failed` (`cannot change to '<repository>'`) and
  `--acknowledge-unrecoverable` could not run, so the record could only be cleared by recreating a
  repository at its path. The dry-run now reports it as `repository-missing`, naming the
  repository, the tree path and the recorded commit; `--apply --id <id>
  --acknowledge-unrecoverable <recorded-commit>` tombstones it as `reconcile-abandoned`. A root
  that exists without `.git` counts as deleted. It is refused as `worktree-path-exists` while the
  tree path, or a pending relocation or removal intent's path, still exists, and it touches nothing
  on disk. Such records are now always reconciliation candidates, so a workspace dry-run no longer
  fails on them or skips them.
- `GitPort` gains `repository_absent`, which defaults to `false`; an embedded adapter that does not
  implement it keeps assessing every record through Git exactly as before. JSON protocol and
  reconciliation versions are unchanged: `repository-missing` travels in the existing refusal field
  and the tombstone reuses the existing `reconcile-abandoned` operation.

## 0.8.1 — 2026-09-26

- The agent guidance `worktree activate --install-agent-guidance` writes to `~/.claude/CLAUDE.md`
  and `~/.codex/AGENTS.md` names the skill as the b10x plugin ships it
  (`worktree:managing-worktrees`, with `/worktree:cleanup` for cleanup) instead of `$worktree`,
  which only exists where `worktree skill` rendered it, and says that `worktree archive` is the
  recovery proof for work that must not be published.

## 0.8.0 — 2026-09-26

- Add `worktree archive [<tree>] [--replace]`. It writes a verified archive of a managed tree to
  `$XDG_STATE_HOME/worktree/archives/<repository>/<id>/` without modifying the tree:
  `commits.bundle` holds every commit HEAD adds over the refs the configured remotes advertise,
  `dirty.patch` recreates every tracked, untracked and ignored file over HEAD, and `manifest.json`
  (new format `worktree.archive/1`) records the tree, HEAD, branch, unique commits, SHA-256 digests
  and the tree id of the archived content. An existing archive is refused as `archive-exists`;
  `--replace` moves it aside and never deletes it.
- `gc` and `finish` accept such an archive as recovery proof of kind `archive` for a tree whose
  commits are on no advertised ref, and for a dirty tree, while the manifest matches the record and
  HEAD, the digests hold, the bundle's own pack carries every object HEAD adds over freshly
  advertised refs, and a dirty tree's content equals the archived state. Mismatches refuse as
  `archive-stale`, `archive-incomplete`, `archive-digest-mismatch`, `archive-bundle-invalid` or
  `archive-invalid`. The dry-run names the archive; apply returns an archived dirty tree to HEAD,
  deleting only files that still match the archive, removes it without force and keeps the archive.
  Trees without an archive are assessed exactly as before.
- **Wire change:** non-hook CLI JSON moves to protocol version 4 and reconciliation JSON to version
  4. Recovery proof gains the `archive` kind and an optional `archive` reference; cleanup
  assessments gain an optional `archive` path. Both are omitted when no archive is involved, and
  stored proofs without them decode unchanged. Inspection 2, hook protocol 1 and configuration
  schema 1 are unchanged.
- Every archive records `worktree_tree`, a fingerprint of the tree's complete on-disk content read
  without filters or index flags, including for a tree Git reports clean; `dirty.patch` is written
  whenever that content differs from HEAD. Removal compares the tree against it before relying on
  the archive. Discarding an archived tree's state resets only the index first and re-hashes each
  tracked file immediately before restoring it, so a later edit is refused, not overwritten.
- **Fix, also on the remote-proof path:** GC and external retirement refuse state Git status does
  not report but removal would destroy: assume-unchanged and skip-worktree entries, staged content
  that differs from both HEAD and the working copy, any `.git` below the tree's root
  (`worktree-hidden-state`), and refs under `refs/worktree/`, `refs/bisect/` and `refs/rewritten/`
  (`worktree-local-refs`). Before this, a published tree holding any of them was removed and the
  state lost. `worktree archive` reports such state as `blocker`.
- The README documents a restore that reproduces archived bytes exactly. It disables every
  attribute conversion through `.git/info/attributes` in a `--no-checkout` clone, because a plain
  `git apply` rewrites line endings under `text eol=crlf`. `--attr-source` does not work for this:
  Git 2.55 `apply` crashes with it on any patch that changes an existing file.
- `GitPort` gains `write_archive`, `verify_archive`, `verify_archived_state`,
  `discard_archived_state` and `hidden_state`, each refusing by default, and `WorktreeManager`
  gains `with_archive_root` and `archive`. An embedded adapter must now implement `hidden_state`,
  or its cleanup refuses as `hidden-state-unobserved`. The Git adapter gains the `sha2` dependency
  for manifest digests.

## 0.7.2 — 2026-09-25

- `worktree gc --apply` removes a tree holding a directory without the owner write bit. Before
  the non-forced `git worktree remove`, the Git adapter gives the owner full access to every
  directory below the proven tree; directory modes are untracked, the walk follows no symlink,
  stays on the tree's filesystem and skips directories another user owns. Before this, Git unlinked
  the tree and then failed to delete its files, leaving a present path that neither `gc` nor
  `reconcile` could finish (#16).
- `gc` finishes such an interrupted removal. A present path that Git no longer links, with a durable
  `remove` intent for the same path, is assessed with fresh remote recovery proof plus proof that
  every remaining file is the intent commit's tracked content. Only its own `.git` file is exempt.
  An exact-id apply then deletes the residue and records the removal. Any other file
  retains the tree as `removal-residue-unproven`; an unlinked tree without intent is still refused.
- A failed `git worktree remove` during `gc --apply` names the rerun command in its refusal.
- `GitPort` gains `verify_removal_residue` and `delete_residue`, each with a refusing default. No
  wire-protocol, schema or other surface version changes.

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
