# Hand-over: agentplugins-improvements, 2026-10-06

## Shipped

| Release | What | Where |
|---|---|---|
| 0.9.0 | `worktree discard-cache` and `worktree finish --discard-cache --archive`: build cache recognised by structure is deleted, everything else is archived | PR #25, main `8f2a577`, tag `0.9.0`, 5 assets |
| 0.10.0 | `worktree sweep`: discards idle trees' cache and archives expired ones; never changes lifecycle, removes a tree or applies GC | PR #26, main `29ce656`, tag `0.10.0`, 5 assets |

Both release runs passed; the Linux archive checksum and `--version` were verified for each.

## State left behind

| Kind | Items |
|---|---|
| Branches | `feat/discard-cache` and `feat/sweep` on origin, both merged into main; `handoff/2026-10-06-agentplugins-improvements` for this file |
| Managed worktrees | none open; the three trees used for this work were retired with `finish --discard-cache --archive` and gc |
| Unpushed commits | none |
| Open PRs | the PR carrying this file, until it merges |
| Open dispatches | none |

## Planning store

- `story:recognised-build-cache-is-discarded` (0.9.0) and `story:idle-trees-are-swept` (0.10.0) were implemented but are still `draft`. The store is `aep.project/1`, which aep 0.68 does not open, so both stories were written by hand and no lifecycle move was recorded. Migrate the store (`aep plan store migrate git --verify`), then move them.
- `story:archived-tree-can-be-restored` is `draft` and not started.

## Next steps

1. One-command restore from an archive (`story:archived-tree-can-be-restored`). A linked worktree shares `info/attributes` with its primary checkout, so the README's restore recipe cannot be reused there; the restore has to write the archived blobs directly.
2. Migrate the planning store, then record the two implemented stories.
3. Archives are never deleted (AGENTS.md invariant); they held 12 GB on the development machine on 2026-10-06. A retention rule would change an invariant and needs a decision first.
4. On Linux, `discard-cache` refuses while any process other than the caller and its ancestors uses the tree. A background command the agent started from inside the tree counts, so agents should finish a tree from outside it or after their background jobs end.

## Notes for the operator's machine

- `worktree` 0.10.0 is installed from the local main checkout.
- A systemd user timer, `worktree-sweep.timer`, runs `worktree sweep --all-profiles` daily at 04:30.
