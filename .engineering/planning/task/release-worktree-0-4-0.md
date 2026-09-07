---
format: aep.planning-md/1
id: task:release-worktree-0-4-0
kind: task
status: implemented
title: Release native storage inspection and explicit lifecycle guidance
owner: codex-hygiene-release
relations:
- delivers: story:native-worktree-storage-inspection
revision: 4
---
## Intent

Release Worktree 0.4.0 with native storage inspection and the stale REBASE_HEAD correction. Make the generated skill explicitly maintain leases when host hooks are absent, retain useful evidence separately from disposable build output, and finish with cleanup or an owned handoff. Authenticate the CI Task installer to avoid anonymous GitHub rate limits.

## Acceptance

- Existing inspection and cleanup safety tests and the complete repository gate pass.
- The generated skill agrees with the CLI and documents lease release before finish.
- Versions, changelog, and install instructions agree on 0.4.0.
- Publish the reviewed source on main and an annotated 0.4.0 release; deliver public documentation through Website.

## Authorization

The operator requested implementation and publication of the Worktree and Agentplugins updates on 2026-09-07. Larger resource budgets and native work-item associations remain separate work.
