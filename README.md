# Worktree

`worktree` gives humans, agents and embedded Rust consumers one safe lifecycle for Git worktrees.
It places trees outside primary checkout collections, records who owns them, proves whether their
commits are recoverable, and refuses cleanup when evidence is incomplete.

The binary is one adapter over the public `b10x-worktree` façade. Applications can inject their own
Git runner, registry and clock; the shipped CLI composes the process-backed Git adapter with an
XDG-state SQLite registry.

## Install

```bash
cargo install --git https://github.com/beyond10x/worktree --tag 0.3.2 b10x-worktree-cli
```

## Use

```bash
worktree create --purpose dependency-refresh
worktree status
worktree finish
worktree gc --repo /path/to/repository --dry-run
worktree gc --repo /path/to/repository --apply --id <reviewed-id>
worktree reconcile --repo /path/to/repository --dry-run
worktree reconcile --repo /path/to/repository --apply --id <reviewed-id>
worktree doctor --check
```

Managed trees default to `$XDG_STATE_HOME/worktree/trees/<profile>/<repository>/<id>`. Activate a
workspace profile with `worktree activate --profile profile.toml --workspace /path/to/workspace`.
Workspace and managed roots are canonical, disjoint paths. Create plans resolve the requested base
to an immutable commit and revalidate the repository, policy-derived destination, and exact Git
worktree membership before changing state.

Generate portable agent guidance from the exact installed command surface:

```bash
worktree skill --out .agents/skills/worktree
worktree skill --out .agents/skills/worktree --check
```

The generated skill and its interface metadata are generator-owned; update them with `worktree
skill`, not by hand.

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

Use `worktree repo list --repo <path>` to inventory linked trees without adopting or deleting them.
Existing trees only become manager-owned through the explicit `repo adopt` command. Hook integrations
can maintain cleanup-blocking leases with `hook session-start`, `hook heartbeat`, and
`hook session-end`.

Dry-runs may assess all candidates or selected ids. Both `gc --apply` and `reconcile --apply`
require one or more exact, reviewed `--id` values; repeat the option to apply more than one result.
Ordinary GC remains restricted to the managed root.

`worktree reconcile` repairs manager-owned legacy and interrupted state without weakening that GC
boundary. It can recover a provisioning record when Git created the exact linked tree, migrate an
active legacy tree into the managed root, retire a finished clean external legacy tree in place,
and tombstone a tree that is already absent. External retirement is the explicit exact-id path for
a legacy tree that cannot be moved across filesystems; it still requires an idle, unlocked, clean
tree, a HEAD stable across final proof/removal observations, and fresh advertised-remote proof.
Applying that action additionally requires `--allow-external-retirement`, so an id reviewed for
migration cannot silently drift into an external deletion.

For legacy state created before 0.3, a finished external tree may still carry a stale relocation
intent. Reconciliation proposes `retire-external` only when that intent names the exact source and
HEAD, Git reports no destination worktree, and the destination path is absent. Durable removal
proof retains the stale intent until successful removal, then completion clears both atomically;
ambiguous or partially moved state is refused.

Relocation and removal intents are durable. Recovery proof is stored before `git worktree remove`,
and registry lifecycle, evidence, and intent completion are committed atomically afterward. If an
operation is interrupted, rerun GC while the path exists or reconciliation once it is absent; the
same dry-run and exact-id apply discipline safely finishes the recorded transition.

Non-hook CLI JSON uses protocol version 2, reconciliation JSON uses version 2, and lifecycle hooks
remain on version 1. Configuration and workspace-policy schemas also remain on version 1.

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
