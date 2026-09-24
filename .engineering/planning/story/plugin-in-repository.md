---
format: aep.planning-md/1
id: story:plugin-in-repository
kind: story
status: draft
title: The worktree agent plugin ships from this repository at the binary version
summary: Move the worktree skill out of agentplugins workspace-hygiene into plugins/worktree, versioned with the binary and installed as worktree@b10x.
revision: 1
---
# Story: the worktree agent plugin ships from this repository

## Goal
The worktree skill ships as the `worktree` agent plugin from `plugins/worktree/` at the binary's
own version, so the skill an agent loads can no longer lag the CLI it describes. Until now it
shipped as `workspace-hygiene` from `beyond10x/agentplugins`, released on that repository's
cadence: plugin 0.10.0 there lacks the `patch-equivalent` recovery-proof guidance that Worktree
0.5.0 generates.

## Acceptance
`task check` fails when either plugin manifest version differs from `[workspace.package] version`,
when the Codex marketplace does not serve `./plugins/worktree`, or when the generated skill in
`plugins/worktree/skills/worktree/` differs from `worktree skill` output; the `b10x` marketplace in
`beyond10x/agentplugins` installs it as `worktree@b10x` at a tag and full commit.

## Scope
- `plugins/worktree/` (manifests, generated skill), `.agents/plugins/marketplace.json`
- `crates/worktree-cli/tests/plugin.rs`, `Taskfile.yml`
- `AGENTS.md`, `README.md`, `b10x.docs.yaml`, `CHANGELOG.md`
