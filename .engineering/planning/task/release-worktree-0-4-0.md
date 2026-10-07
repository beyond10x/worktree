---
format: aep.planning-md/3
id: task:release-worktree-0-4-0
kind: task
status: implemented
title: Release native storage inspection and explicit lifecycle guidance
owner: codex-hygiene-release
relations:
- delivers: story:native-worktree-storage-inspection
revision: 4
transitions:
- {from: "draft", to: "proposed", at: "2026-09-07T12:16:30Z", actor: "human:timo", revision: 2, imported: true}
- {from: "proposed", to: "active", at: "2026-09-07T12:16:30Z", actor: "human:timo", revision: 3, imported: true}
- {from: "active", to: "implemented", at: "2026-09-07T12:21:04Z", actor: "human:timo", revision: 4, decided_on: {"recorded":{"test_result":1}}, imported: true}
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
