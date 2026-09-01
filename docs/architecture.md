# Architecture

The workspace separates lifecycle decisions from operating-system adapters. Consumers embed the
façade and inject ports; the shipped CLI is one composition root.

```mermaid
flowchart TD
    domain["b10x-worktree-domain<br/>values · plans · refusals"]
    facade["b10x-worktree<br/>WorktreeManager · ports"]
    git["b10x-worktree-git<br/>process Git adapter"]
    state["b10x-worktree-state<br/>SQLite · XDG config"]
    cli["b10x-worktree-cli<br/>CLI · hooks · skill renderer"]
    harness["future: Harness Git toolchain"]

    facade --> domain
    git --> facade
    state --> facade
    cli --> git
    cli --> state
    cli --> facade
    harness -. injects ports .-> facade
```

The façade never depends on concrete Git, database, CLI, Atlas, or agent runtime behavior. The
domain crate performs no I/O. This dependency direction lets an embedded consumer replace process
execution and persistence without reimplementing cleanup policy.

## Cleanup proof

```mermaid
flowchart LR
    candidate["finished or expired record"] --> owned{"manager-owned<br/>and in policy root?"}
    owned -->|no| refuse["typed refusal"]
    owned -->|yes| idle{"no live lease<br/>or Git lock?"}
    idle -->|no| refuse
    idle -->|yes| clean{"tracked + untracked<br/>state clean?"}
    clean -->|no| refuse
    clean -->|yes| fetch["fetch remotes + tags"]
    fetch --> recovery{"HEAD reachable from<br/>remote branch or tag?"}
    fetch -->|offline / error| refuse
    recovery -->|no| refuse
    recovery -->|yes| revalidate["repeat observation<br/>immediately before mutation"]
    revalidate --> remove["git worktree remove<br/>without --force"]
    remove --> evidence["record recovery and<br/>removal evidence"]
```

Dry-run garbage collection traverses the same proof but stops before revalidation and removal.
Unknown, offline, dirty, locked, live, local-only, and out-of-root states retain the tree.

## Durable state

Activated profiles live under `$XDG_CONFIG_HOME/worktree/config.toml`. The ownership registry,
leases, lifecycle records, and cleanup evidence live in
`$XDG_STATE_HOME/worktree/registry.sqlite3`. Managed worktrees default to
`$XDG_STATE_HOME/worktree/trees/<profile>/<repository>/<id>`.

The JSON and hook surface declares protocol version 1. Wire-shape changes require a new protocol
version; adding Rust API implementations can remain semver-compatible when existing contracts do
not change.
