# Worktree

`worktree` gives humans, agents and embedded Rust consumers one safe lifecycle for Git worktrees.
It places trees outside primary checkout collections, records who owns them, proves whether their
commits are recoverable, and refuses cleanup when evidence is incomplete.

The binary is one adapter over the public `b10x-worktree` façade. Applications can inject their own
Git runner, registry and clock; the shipped CLI composes the process-backed Git adapter with an
XDG-state SQLite registry.

## Install

```console
cargo install --git https://github.com/beyond10x/worktree --tag 0.1.0 b10x-worktree-cli
```

## Use

```console
worktree create --purpose dependency-refresh
worktree status
worktree finish
worktree gc --dry-run
worktree doctor --check
```

Managed trees default to `$XDG_STATE_HOME/worktree/trees/<repository>/<id>`. Activate a stricter
workspace profile with `worktree activate --profile profile.toml --workspace /path/to/workspace`.

Generate portable agent guidance from the exact installed command surface:

```console
worktree skill --out .agents/skills/worktree
```

The command refuses dirty, unpublished, locked, live, out-of-root or remotely unverifiable cleanup
candidates. It never uses forced worktree removal.

Use `worktree repo list --repo <path>` to inventory linked trees without adopting or deleting them.
Existing trees only become manager-owned through the explicit `repo adopt` command. Hook integrations
can maintain cleanup-blocking leases with `hook session-start`, `hook heartbeat`, and
`hook session-end`.

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
