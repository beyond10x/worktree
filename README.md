# Worktree

`worktree` gives humans, agents and embedded Rust consumers one safe lifecycle for Git worktrees.
It places trees outside primary checkout collections, records who owns them, proves whether their
commits are recoverable, and refuses cleanup when evidence is incomplete.

The binary is one adapter over the public `b10x-worktree` façade. Applications can inject their own
Git runner, registry and clock; the shipped CLI composes the process-backed Git adapter with an
XDG-state SQLite registry.

## Install

Prebuilt, from the [latest release](https://github.com/beyond10x/worktree/releases/latest):
`worktree-<version>-<target>.tar.gz` for `x86_64`/`aarch64` Linux and macOS, checked against
`SHA256SUMS`. Or build it:

```bash
cargo install --git https://github.com/beyond10x/worktree --tag 0.7.0 b10x-worktree-cli
```

[`b10x`](https://github.com/beyond10x/agentplugins) installs either way and keeps it current.

## Use

```bash
worktree create --purpose dependency-refresh
worktree hook session-start --path <tree> --session <session-id>
worktree inspect --repo /path/to/repository
worktree status
worktree hook heartbeat --path <tree> --session <session-id>
# Publish wanted changes.
worktree hook session-end --path <tree> --session <session-id>
# Review which ignored directories are recognised build cache; everything else is kept.
worktree discard-cache <tree> --dry-run
# Delete that cache, archive whatever no remote ref recovers, and finish.
worktree finish --discard-cache --archive <tree>
worktree gc --repo /path/to/repository --dry-run --id <reviewed-id>
worktree gc --repo /path/to/repository --apply --id <reviewed-id>
# Archives a remote now fully holds; refused ones say why and are kept.
worktree prune-archives --repo /path/to/repository --dry-run
worktree prune-archives --repo /path/to/repository --apply --id <reviewed-archive-directory>
# Strip cargo build output from older archives; review, then rewrite only reviewed ones.
worktree prune-archives --strip-build-output --repo /path/to/repository --dry-run
worktree prune-archives --strip-build-output --repo /path/to/repository --apply --id <reviewed-archive-directory>
worktree reconcile --repo /path/to/repository --dry-run
worktree reconcile --repo /path/to/repository --apply --id <reviewed-id>
worktree doctor --check
```

Managed trees default to `$XDG_STATE_HOME/worktree/trees/<profile>/<repository>/<id>`. Activate a
workspace profile with `worktree activate --profile profile.toml --workspace /path/to/workspace`;
add `--install-agent-guidance` to write a managed guidance block into `~/.codex/AGENTS.md` and
`~/.claude/CLAUDE.md`, replacing only the block between its markers. Workspace and managed roots
are canonical, disjoint paths. Create plans resolve the requested base to an immutable commit and
revalidate the repository, policy-derived destination, and exact Git worktree membership before
changing state.

`worktree discard-cache` recognises build cache by structure, never by a directory's name: a Cargo
profile (it holds `.fingerprint/`) inside a target carrying a valid `CACHEDIR.TAG`, that target's
`tmp/` test scratch (Cargo's `CARGO_TARGET_TMPDIR`, whatever it holds) when the target holds a
profile, `node_modules` at or below a tracked npm, Yarn, pnpm or Bun lockfile, a virtual environment (`pyvenv.cfg`) beside
a tracked Python manifest, and a tagged `.pytest_cache`, `.mypy_cache` or `.ruff_cache`. Cargo
writes the tag only when it creates the target itself, so a `target/` made before the first build
has none; such a directory counts as tagged when a tracked `Cargo.toml` sits beside it and a
child other than `tmp/` holds both `.fingerprint/` and `deps/`. Every other
ignored entry, including records written inside `target/`, is kept and named in the report. It
follows no symbolic link, and it refuses while a session lease is live, Git locks the tree, or
another process has its working directory, executable or an open file inside the tree (observed
through `/proc` on Linux; elsewhere the report says processes were not observed).

`worktree sweep --all-profiles` does the same for every tree without a live lease that shows no
activity, in the registry or in its own Git index and HEAD, for `--idle-days` (default 1). For a
tree past the profile's expiry, or already finished, it also writes an archive of whatever no
advertised ref recovers, refusing as `archive-too-large` above `--max-archive-mib` (default 1024).
It never changes lifecycle and never removes a tree: `worktree gc` stays review-bound. Run it from
a systemd user timer:

```ini
# ~/.config/systemd/user/worktree-sweep.service
[Service]
Type=oneshot
ExecStart=%h/.local/bin/worktree sweep --all-profiles

# ~/.config/systemd/user/worktree-sweep.timer
[Timer]
OnCalendar=*-*-* 04:30:00
Persistent=true

[Install]
WantedBy=timers.target
```

Print portable agent guidance from the exact installed command surface. Without `--out` it goes
to standard output and no file is written:

```bash
worktree skill
```

To install it as files (`SKILL.md` and `agents/openai.yaml`) or check an installed copy, name the
directory:

```bash
worktree skill --out <dir>
worktree skill --out <dir> --check
```

The written skill and its interface metadata are generator-owned; update them with `worktree skill
--out <dir>`, not by hand.

The agent plugin is `worktree@b10x` in [`beyond10x/agentplugins`](https://github.com/beyond10x/agentplugins).

Before removal, the manager treats tracked, untracked, and ignored files as dirty and checks Git
worktree locks, operational lock files, and paused merge/rebase/sequencer state. It refuses live
leases, non-members, a HEAD that changes while proof and removal intent are collected, ambiguous
or symlink-redirected paths, and incomplete remote evidence. Finish persists the final HEAD
atomically with the lease check. Cleanup of an expired active tree first claims its lifecycle
atomically, which prevents new sessions from racing the removal.

Recovery proof is based only on exact refs currently advertised by configured remotes. Branches,
tags, pull-request refs such as `refs/pull/*`, and custom namespaces can all prove recovery. The
manager fetches only required missing objects without creating local refs, re-reads the
advertisements, and proves that the exact HEAD is reachable. Local-only tags and stale or
fabricated remote-tracking refs do not count. Replacement refs and grafted ancestry are disabled;
repository graft files cause refusal. Offline, changed, or ambiguous advertisements cause refusal.

Work that was rebased or cherry-picked before it was merged has new commit ids on the remote, so
its exact HEAD is reachable from no advertised ref. The manager then accepts a second, recorded
proof kind, `patch-equivalent`: one advertised ref must carry a commit with a whitespace-exact
identical patch for every commit that no advertised ref holds, and Git's own cherry-pick
equivalence must agree. A unique root, merge, or empty commit has no single patch another commit
could carry and defeats the proof. Binary changes compare by their full binary patch. The proof
records the refs and the equivalent commits; the local branch and its commit ids are not removed,
only the linked tree.

Work that must not be published can be retired against a local archive instead. `worktree archive
<tree>` writes `$XDG_STATE_HOME/worktree/archives/<repository>/<id>/` with mode 0700 and never
modifies the tree, its index or the repository's refs:

| File | Content |
|---|---|
| `commits.bundle` | every commit HEAD adds over the refs the configured remotes advertise, as `refs/worktree-archive/head`; absent when there are none |
| `dirty.patch` | a binary patch from HEAD that recreates every tracked, untracked and ignored file byte for byte, except cargo build layout; absent when the tree's content equals HEAD |
| `nested-<n>.tar` | one per nested Git repository in the tree's files, `n` from 1 in path-byte order: its root directory and every entry below it, `.git` included, with permission bits, modification times and symlink targets; names are relative to the tree root |
| `manifest.json` | format `worktree.archive/1`, `worktree.archive/2` when it images nested repositories, or `worktree.archive/3` when it left cargo build layout out: tree id, repository root, path, HEAD, branch, the unique commits, each file's SHA-256 and size, `worktree_tree` (the fingerprint of the tree's on-disk content outside every nested repository and the build layout left out), `nested_repositories` (each root's path, image file, fingerprint and entry count; only in `/2` and `/3`), `created_at`, and `build_output` (only in `/3`: per target its `path`, `origin` (`archive` or `strip`), `files` and `bytes`, and the totals) |

Cargo's own build layout is left out, because the next build recreates it from tracked sources.
Below a cargo target that `discard-cache` recognises by structure (a valid `CACHEDIR.TAG`, or an
untagged `target/` beside a tracked `Cargo.toml` holding a full profile), a file neither HEAD nor
the index tracks is build layout when its first component below the target is `debug`, `release`,
`.rustc_info.json`, `tmp`, a `CACHEDIR.TAG` carrying Cargo's signature, or a profile directory (one
holding `.fingerprint/`; also `<target>/<triple>/<profile>/`). The target is recognised on disk by
that structure and a profile, also when a file Git tracks below it keeps `discard-cache` from
reaching it; the tracked file stays archived. That covers `deps/`, `build/`, `.fingerprint/`, `incremental/`
and `*.d`. Everything else below the target, such as `target/ess-conformance/report.json`, and
every other ignored directory stays in the archive. The command prints `left out <files> file(s),
<bytes> bytes of cargo build output under <target>`. An archive that left nothing out is written
exactly as before, format 1 or 2.

The fingerprint is a Git tree id computed from every file on disk except the build layout left
out, read without filters, index flags or attributes. It is recorded for every archive, including
one of a tree Git reports clean, because Git status can be told not to look at a file. Before
publishing an archive the command runs
`git bundle verify`, indexes the bundle's pack on its own and checks that it holds every object
those commits need, applies the patch to HEAD in a scratch index and compares the result with the
fingerprint, and re-reads every digest. An existing archive is refused as `archive-exists`;
`--replace` moves it to `<id>.superseded-<time>` and never deletes it.

A directory Git lists only as `<path>/`, whose `.git` is a real directory, is a nested repository,
typically a test fixture kept in an ignored evidence directory. The patch cannot hold it, so the
archive writes a tar image of it. Its fingerprint is the SHA-256 of a canonical listing of the
root and every entry below it (kind, permission bits, size, the SHA-256 of a file's content or a
symlink's target, path), sorted by path bytes. The image read back, and the directory on disk
before and after writing, must all give that fingerprint, or nothing is published. A nested
repository that depends on state outside its directory is refused as `archive-unsupported-entry`:
a `.git` that is a file or symlink (submodule or linked worktree), `commondir`, non-empty
`objects/info/alternates`, `worktrees/` entries or a symlink inside any `.git` below the root, and
a root at or below which HEAD or the index tracks anything (a submodule records mode 160000).

GC and finish consult an archive only when one exists. A clean tree with no remote proof, or a
dirty tree, is then eligible with recovery kind `archive` while all of this still holds: the
manifest names this record and its current HEAD, every file has its recorded digest, freshly
advertised refs plus the bundle's own objects hold every commit HEAD adds, and the tree's complete
content still has the archived fingerprint. Every image still has its recorded digest, the nested
repositories on disk are exactly those imaged, and each still has its recorded fingerprint. A
dirty tree is never covered by remote refs. Any
mismatch refuses by name: `archive-stale` (HEAD moved, or files changed), `archive-incomplete` (a
missing bundle, or a bundle lacking an object), `archive-digest-mismatch`, `archive-bundle-invalid`
or `archive-invalid`. Leases, locks and paused Git operations refuse exactly as without an archive.
The dry-run names the archive.

Apply resets an archived dirty tree's index to HEAD, then restores each tracked file Git reports as
changed only after re-hashing it and finding the archived bytes, deletes each untracked or
ignored file only when it still matches the archive, and deletes each imaged nested repository
bottom-up, re-observing every entry against its image immediately before deleting it. For a
`worktree.archive/3` archive it deletes each left-out file as cache only after re-observing it,
immediately before deletion, as build layout below the same recognised target; build output grown
since the archive is deleted the same way. Layout below a target the archive did not record, or a
left-out file that is no longer build layout (its profile lost `.fingerprint/`, its target is no
longer recognised), changes the fingerprint and refuses as `archive-stale`. A directory it must
empty is made owner-writable first, as Git's own removal does. It then removes the tree without
`--force`. The archive and its images stay.

Some state is invisible to `git status`, and neither remote refs nor an archive hold it, so every
removal, with or without an archive, is refused while it exists; `worktree archive` reports it as
`blocker`:

| Refusal | State |
|---|---|
| `worktree-hidden-state` | an entry marked assume-unchanged or skip-worktree; staged content that differs from both HEAD and the working copy; any `.git` directory or file below the tree's root, except inside a nested repository the archive a removal relies on images |
| `worktree-local-refs` | a ref under `refs/worktree/`, `refs/bisect/` or `refs/rewritten/`, which Git deletes with the tree |

Restore into a fresh `--no-checkout` clone of a remote that still has the advertised refs. Every
attribute conversion is switched off, so `switch` and `apply` write the archived bytes rather
than converted ones. `.git/info/attributes` takes precedence over every `.gitattributes`. Left
enabled, a `text eol=crlf` attribute rewrites a lone LF even in a file the patch recreates:

```bash
git clone --no-checkout <remote> restored && cd restored
printf '* -text -eol -filter -ident -working-tree-encoding\n' > .git/info/attributes
git fetch <archive>/commits.bundle refs/worktree-archive/head:refs/heads/restored
raw() { git -c core.autocrlf=false -c core.fileMode=true -c core.symlinks=true "$@"; }
raw switch restored
raw apply --binary --whitespace=nowarn <archive>/dirty.patch   # only when the archive has one
tar -xpf <archive>/nested-1.tar -C .                            # once per nested-<n>.tar
```

`-p` keeps the archived permission bits whatever the umask; owners are restored only as root.

Delete `.git/info/attributes` afterwards if the clone should convert line endings again. Do not
use `--attr-source` or `GIT_ATTR_SOURCE` instead: Git 2.55 `apply` crashes with either one on any
patch that changes an existing file.

The bundle's prerequisites are the advertised commits it was cut against, so restoring needs a
remote that still has them. Submodules, nested repositories that depend on state outside their
directory, special files and paths containing a newline are refused as `archive-unsupported-entry`.
Every ignored file other than cargo build layout is archived; discard other disposable output
first with `worktree discard-cache`.

An archive outlives its tree, and only `worktree prune-archives` deletes one. It lists each archive
directory below `$XDG_STATE_HOME/worktree/archives/<repository>/` (`<id>` and superseded
`<id>.superseded-<n>`) as `directory, worktree id, bytes, verdict, reason`, then the totals of bytes
removable and refused. Without `--id` it assesses the archives whose manifest names the repository
`--repo` resolves to; `--scope profile` assesses every archive. Files there that are not archive
directories (a `.tsv`, a `.bundle`) and directories whose name starts with `.` are listed as
skipped and never deleted. Verdicts are checked in this order:

| Verdict | When |
|---|---|
| `InvalidManifest` | `manifest.json` is missing, unreadable or of an unknown format |
| `UnrecordedContent` | the directory holds an entry the manifest does not name as a regular file |
| `TreeStillPresent` | the registered tree path exists; the archive may still be its recovery proof |
| `NestedRepositories` | the archive images a nested repository |
| `UncommittedState` | the archive holds `dirty.patch` |
| `RemoteProofUnavailable` | the repository is gone, has no remote, or a remote does not answer |
| `CommitsNotOnRemote` | HEAD or a unique commit is an ancestor of no freshly advertised ref; a rebased or cherry-picked copy does not count |
| `Removable` | none of the above |

Dry-run is the default. `--apply` requires `--id <directory>` (or `<repository>/<directory>` when
two repositories hold the same name) copied from a dry-run. Each is re-assessed immediately before
deletion; a removable one loses the files its manifest names, then the manifest, then the empty
directory, never recursively, and prints `removed <directory> <bytes>` and `freed <bytes> bytes`. A
refused one is kept, its verdict printed, and the run exits non-zero after the others. No flag makes
a refused archive removable: an archive whose commits no remote holds is the only copy of them, and
deleting it stays the operator's decision, by hand.

Archives written before build output was left out hold it in `dirty.patch` and are refused as
`UncommittedState`. `worktree prune-archives --strip-build-output` removes it, with the same
`--repo`, `--scope` and `--id` selection, and never deletes an archive. A dry-run, the default,
streams each patch and lists `directory, worktree id, bytes, verdict`, then the bytes and sections
it would strip, the sections, bytes and nested images that stay, and the prune verdict the archive
would have afterwards. Only a section that purely adds an untracked file is stripped: `new file
mode`, a path, C-unquoted as Git writes it, that no other section names and that HEAD's tree, read
from Git, does not track, and that is build layout below a directory the patch's own pure
additions recognise as a target (a Cargo-signed `<t>/CACHEDIR.TAG` or `<t>/<p>/.fingerprint/…`;
`<t>/tmp/` only beside a profile). A section whose path cannot be decoded, both halves of a type
change, every other change to a tracked file and every nested image stay.
Verdicts, in this order:

| Verdict | When |
|---|---|
| `InvalidManifest` | `manifest.json` is missing, unreadable or of an unknown format |
| `UnrecordedContent` | the directory holds an entry the manifest does not name as a regular file |
| `TreeStillPresent` | the registered tree path exists; the archive may still be its recovery proof |
| `RepositoryGone` | the repository is gone, so a new patch cannot be verified over HEAD |
| `PatchUnusable` | `dirty.patch` lost its digest, holds bytes before its first `diff --git`, or the stripped patch does not apply over HEAD |
| `NothingToStrip` | no patch, or no section of it adds build layout |
| `Strippable` | at least one section adds build layout |

`--apply` requires `--id`. Each is re-assessed; the new patch is written in a scratch directory
beside the archive, applied over HEAD in a scratch index (never the repository's own index), and
`worktree_tree` recomputed from it. Only then is `dirty.patch` replaced, or deleted when nothing
remains (`patch: null`), and the `worktree.archive/3` manifest written last, recording what was
stripped as `build_output` with origin `strip`. It prints `stripped <directory> freed <bytes>
bytes` and `freed <bytes> bytes in total`; anything other than `Strippable` is left unchanged and
the run exits non-zero. An archive whose remaining state a remote holds is then `Removable` under
the rule above, and still goes only with `worktree prune-archives --apply --id`.

Use `worktree repo list --repo <path>` to inventory linked trees without adopting or deleting them.
Existing trees only become manager-owned through the explicit `repo adopt` command. Hook integrations
can maintain cleanup-blocking leases with `hook session-start`, `hook heartbeat`, and
`hook session-end`. When hooks are absent, run them explicitly and renew the lease before expiry.
Release your own lease before finishing. Build output and ignored files remain on disk until their
owner preserves useful evidence and removes the exact disposable directories.

`worktree inspect --repo <path>` reports actual Git state, ignored files, storage, live leases,
recorded activity and retention blockers for that repository, including active trees. Add
`--workspace` to expand the scope, repeat `--id` to narrow it, and use `--refresh` for fresh
remote recovery evidence. Storage scans are bounded and flag incomplete results; reported bytes
are observations, not guaranteed reclaimable space. Inspection does not change lifecycle or infer
story completion or abandonment. Cleanup still requires a reviewed GC assessment.

Dry-runs may assess all candidates or selected ids. Without ids, `gc --repo <path>` assesses only
the records of the repository that path resolves to (`--scope repo`, the default); add
`--scope profile` to assess every record under the activated profile that repository selects.
`--id` assesses the named records whatever the scope. Both
`gc --apply` and `reconcile --apply` require one or more exact, reviewed `--id` values; repeat the
option to apply more than one result. Ordinary GC remains restricted to the managed root.

`worktree reconcile` repairs manager-owned legacy and interrupted state without weakening that GC
boundary. It can recover a provisioning record when Git created the exact linked tree, migrate an
active legacy tree into the managed root, retire a finished clean external legacy tree in place,
and tombstone a tree that is already absent. External retirement is the explicit exact-id path for
a legacy tree that cannot be moved across filesystems; it still requires an idle, unlocked, clean
tree, a HEAD stable across final proof/removal observations, and fresh advertised-remote proof.
Applying that action additionally requires `--allow-external-retirement`, so an id reviewed for
migration cannot silently drift into an external deletion.

A missing record whose recorded commit no ref holds stays refused. Once its owner has established
that the commit is gone for good, `reconcile --apply --id <reviewed-id>
--acknowledge-unrecoverable <commit>` tombstones it. The command refuses while any local branch,
tag, remote-tracking ref or remote advertisement still contains that commit, deletes nothing, and
records no recovery proof.

A record whose repository was deleted — its recorded root is gone, or is a directory without
`.git` — has nothing left for Git to check. The dry-run reports it as `repository-missing`, naming
the repository, the tree path and the recorded commit; pass any live repository of the same
workspace as `--repo`. `reconcile --apply --id <reviewed-id> --acknowledge-unrecoverable <commit>`
tombstones it as `reconcile-abandoned` only when that commit is the recorded one and the tree path,
and every path a pending relocation or removal intent names, is already absent. A tree that still
exists is refused as `worktree-path-exists` and left where it is. Nothing on disk is touched.

For legacy state created before 0.3, a finished external tree may still carry a stale relocation
intent. Reconciliation proposes `retire-external` only when that intent names the exact source and
HEAD, Git reports no destination worktree, and the destination path is absent. Durable removal
proof retains the stale intent until successful removal, then completion clears both atomically;
ambiguous or partially moved state is refused.

Relocation and removal intents are durable. Recovery proof is stored before `git worktree remove`,
and registry lifecycle, evidence, and intent completion are committed atomically afterward. If an
operation is interrupted, rerun GC while the path exists or reconciliation once it is absent; the
same dry-run and exact-id apply discipline safely finishes the recorded transition.

Non-hook CLI JSON uses protocol version 4, reconciliation JSON uses version 4, inspection reports
use `worktree.inspection/2`, archive manifests use `worktree.archive/1`, or `worktree.archive/2`
when they image nested repositories (a 0.11 reader refuses `/2` as `archive-invalid`), or
`worktree.archive/3` when they left cargo build layout out (a 0.14 reader refuses `/3` as
`archive-invalid`), and
lifecycle hooks remain on version 1. Version 3 and inspection 2 add the recovery proof `kind` and `equivalent_commits`
fields. Version 4 adds the `archive` command, the `archive` recovery kind with its `archive`
reference, and the cleanup assessment's `archive` path; both fields are omitted when no archive is
involved. Configuration and workspace-policy schemas also remain on version 1.

## Embed

Depend on `b10x-worktree` from this Git repository and implement `GitPort`, `RegistryPort`, and
`Clock`, or compose the shipped adapters. `WorktreeManager` is the stable application boundary;
the CLI has no additional lifecycle policy. This keeps future Harness integration on a library
surface instead of screen-scraping a subprocess.

See [docs/architecture.md](docs/architecture.md) for the dependency direction and mutation proof.

## Workspace

- `b10x-worktree-domain`: I/O-free values and decisions.
- `b10x-worktree`: public application façade and ports.
- `b10x-worktree-git`: process-backed Git adapter.
- `b10x-worktree-state`: SQLite registry and XDG configuration.
- `b10x-worktree-cli`: `worktree` binary, hook protocol and skill renderer.

<!-- b10x-docs:start -->
## Documentation

[Worktree documentation](https://beyond10x.github.io/docs/worktree/) · [Start](https://beyond10x.github.io/) · [Ecosystem](https://beyond10x.github.io/ecosystem/) · [Impact](https://beyond10x.github.io/changes/) · [Releases](https://beyond10x.github.io/releases/)
<!-- b10x-docs:end -->
