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
# Publish wanted changes and remove this task's disposable build output.
worktree hook session-end --path <tree> --session <session-id>
# Or keep unpublished work in a verified local archive instead of on a remote.
worktree archive <tree>
worktree finish <tree>
worktree gc --repo /path/to/repository --dry-run --id <reviewed-id>
worktree gc --repo /path/to/repository --apply --id <reviewed-id>
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

Generate portable agent guidance from the exact installed command surface:

```bash
worktree skill --out .agents/skills/worktree
worktree skill --out .agents/skills/worktree --check
```

The generated skill and its interface metadata are generator-owned; update them with `worktree
skill`, not by hand.

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
| `dirty.patch` | a binary patch from HEAD that recreates every tracked, untracked and ignored file byte for byte; absent when the tree's content equals HEAD |
| `manifest.json` | format `worktree.archive/1`: tree id, repository root, path, HEAD, branch, the unique commits, each file's SHA-256 and size, `worktree_tree` (the fingerprint of the tree's complete on-disk content), and `created_at` |

The fingerprint is a Git tree id computed from every file on disk, read without filters, index
flags or attributes. It is recorded for every archive, including one of a tree Git reports clean,
because Git status can be told not to look at a file. Before publishing an archive the command runs
`git bundle verify`, indexes the bundle's pack on its own and checks that it holds every object
those commits need, applies the patch to HEAD in a scratch index and compares the result with the
fingerprint, and re-reads every digest. An existing archive is refused as `archive-exists`;
`--replace` moves it to `<id>.superseded-<time>` and never deletes it.

GC and finish consult an archive only when one exists. A clean tree with no remote proof, or a
dirty tree, is then eligible with recovery kind `archive` while all of this still holds: the
manifest names this record and its current HEAD, every file has its recorded digest, freshly
advertised refs plus the bundle's own objects hold every commit HEAD adds, and the tree's complete
content still has the archived fingerprint. A dirty tree is never covered by remote refs. Any
mismatch refuses by name: `archive-stale` (HEAD moved, or files changed), `archive-incomplete` (a
missing bundle, or a bundle lacking an object), `archive-digest-mismatch`, `archive-bundle-invalid`
or `archive-invalid`. Leases, locks and paused Git operations refuse exactly as without an archive.
The dry-run names the archive.

Apply resets an archived dirty tree's index to HEAD, then restores each tracked file Git reports as
changed only after re-hashing it and finding the archived bytes, and deletes each untracked or
ignored file only when it still matches the archive; it then removes the tree without `--force`.
The archive stays.

Some state is invisible to `git status`, and neither remote refs nor an archive hold it, so every
removal, with or without an archive, is refused while it exists; `worktree archive` reports it as
`blocker`:

| Refusal | State |
|---|---|
| `worktree-hidden-state` | an entry marked assume-unchanged or skip-worktree; staged content that differs from both HEAD and the working copy; any `.git` directory or file below the tree's root |
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
```

Delete `.git/info/attributes` afterwards if the clone should convert line endings again. Do not
use `--attr-source` or `GIT_ATTR_SOURCE` instead: Git 2.55 `apply` crashes with either one on any
patch that changes an existing file.

The bundle's prerequisites are the advertised commits it was cut against, so restoring needs a
remote that still has them. Submodules, nested repositories, special files and paths containing a
newline are refused as `archive-unsupported-entry`. Every ignored file is archived; there is no
policy for disposable output yet, so remove build output first.

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

Dry-runs may assess all candidates or selected ids. Without ids, `gc --repo <path>` assesses every
record under the activated profile that repository selects, not only that repository. Both
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
use `worktree.inspection/2`, archive manifests use `worktree.archive/1`, and lifecycle hooks remain
on version 1. Version 3 and inspection 2 add the recovery proof `kind` and `equivalent_commits`
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
