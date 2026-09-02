# Changelog

## Unreleased

## 0.2.0 — 2026-09-02

- Add reviewed legacy reconciliation that migrates adopted linked trees into the managed root and
  records safely missing worktrees without weakening cleanup containment.

## 0.1.0 — 2026-09-01

- Add the typed multi-crate worktree lifecycle library and CLI.
- Add XDG-state placement, SQLite ownership and lease records, recoverability-gated cleanup, and
  workspace audits.
- Add hook protocol version 1 and deterministic `worktree skill` generation.
