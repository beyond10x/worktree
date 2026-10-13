---
format: aep.planning-md/3
id: story:upgrade-all-dependencies
kind: story
status: active
title: Every dependency is on its newest release
revision: 3
transitions:
- {from: "draft", to: "proposed", at: "2026-10-13T14:10:31Z", actor: "human:timo", revision: 2}
- {from: "proposed", to: "active", at: "2026-10-13T14:10:31Z", actor: "human:timo", revision: 3}
---
## Outcome

Every dependency of the workspace is on its newest release: each Cargo dependency (requirement
and lock), the GitHub Actions the workflows pin, and the shared source gate the repository calls.
The full gate stays green on the result, and the release that follows names every bumped version
in its notes.

## Why

The operator asked on 2026-10-13 for every dependency of this repository to be upgraded.
`cargo upgrade --dry-run --incompatible` on `main` 8ee5504 reported three requirements behind
their newest release: `rusqlite` 0.37 (newest 0.40.2), `sha2` 0.10 (0.11.0) and `toml` 0.9
(1.1.8). `gate.yml` pins `actions/checkout@v4` and `arduino/setup-task@v2` (newest v7.0.1 and
v3.0.0); `release.yml` pins `dtolnay/rust-toolchain` at a `stable` commit that is no longer the
branch head, and `upload-artifact`/`download-artifact` one patch behind; `shared-gates.yml`
calls the shared gate at a commit 87 commits behind its 0.1.18 release.

## Contract

No behaviour changes. The ESS specification (`.engineering/specs/worktree-inspection/`) models
behaviour, not dependencies, so it is unchanged; it validates with the newest `ess`.

- `Cargo.toml` workspace requirements name the newest release of every dependency;
  `cargo upgrade --dry-run --incompatible` reports nothing to upgrade.
- `Cargo.lock` is refreshed with `cargo update`; `cargo update --dry-run` reports nothing.
- `rust-version` names the toolchain the dependencies need, at most the current stable.
- Each `uses:` in the repository-owned workflows (`gate.yml`, `release.yml`, `shared-gates.yml`)
  is the newest release of that action, pinned by commit where the file already pins by commit.
  The workflows `atlas docs reconcile` generates are not edited by hand.
- Archive digests and manifests keep their bytes: `sha2` 0.11 produces the same SHA-256 hex
  strings, shown by the existing archive tests.
- `CHANGELOG.md`'s entry for the release lists each bumped dependency with old and new version.

## Acceptance

- `task check` passes on the integration head (fmt, clippy `-D warnings`, every test).
- The pull request's CI (`gate`, shared source gate, docs check) is green.
- The release's GitHub Release body lists the bumped versions.
