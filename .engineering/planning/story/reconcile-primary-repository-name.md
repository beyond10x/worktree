---
format: aep.planning-md/1
id: story:reconcile-primary-repository-name
kind: story
status: draft
title: Support canonical primary repository names without manual registry edits
owner: worktree-maintainers
relations:
- informed_by: story:native-worktree-storage-inspection
revision: 1
---
## Consumer request
A post-migration engine checkout retained a temporary local primary name. The operator wants the canonical name restored through managed reconciliation, while preserving every linked checkout, lease, immutable source identity and recovery proof. The installed Worktree0.4.0 CLI offers reconciliation of linked-tree provisioning, adoption and missing records, but exposes no primary-checkout rename or repository-root rebinding command. Current published main has the same relevant command boundary. Manual directory moves or registry edits are not an admitted consumer workaround.

## Owner action and acceptance
Worktree owners should document an existing supported procedure if one exists, or provide a bounded primary relocation/rebinding contract. A dry run must enumerate affected repository roots, linked Git worktree metadata, registry rows and leases, exact destination conflicts and recovery proof. Applying an exact reviewed plan must preserve dirty work and native ignored evidence, refuse live/ambiguous state and recover safely after interruption without force removal or a manually edited registry. Retain old evidence identities rather than pretending they were created at the new path. Consumers separately update their executable/document deployment manifests and configuration only after qualification and normal service drain.

## Scope and handoff
This is a single owner request. The engine session does not implement or release Worktree, move a primary checkout, change private registry rows, clear leases or retire retained evidence. Worktree owns the lifecycle capability and its deterministic verification. Link this request to native-worktree-storage-inspection for the already admitted inspection/recovery boundaries. No decomposition panel is needed for one request. The consumer can continue qualified service work while this explicit naming follow-up is assigned.
