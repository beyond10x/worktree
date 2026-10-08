---
format: aep.planning-md/3
id: review-result:adversary-untagged-target-pass-1
kind: review-result
status: active
title: Adversary, untagged Cargo target, pass 1
summary: '3 findings on 690220c, all introduced: tmp/ counted as a full profile, bad-signature tag deleted, root-only manifest mutant survives'
relations:
- reviews: story:untagged-cargo-target-is-cache
revision: 1
---
needs-revision

```
unit: story:untagged-cargo-target-is-cache, commit 690220c in tree unit-untagged-target, plus one untracked test file
verdict: INFEASIBLE (2 contract breaks that delete, on states nothing found creates); CONFIRMED 1 (suite gap, note)
cases: executed 104→114, red 3
origin: introduced 3 / pre-existing 0 / undecided 0
wrote-outside-worktree: 1 (sccache's shared cache, written by the configured rustc-wrapper during builds)
needs-coordinator: no session lease taken; the brief forbids `worktree` lifecycle commands, and `worktree hook` is that lifecycle protocol
```

Cases, in `crates/worktree-cli/tests/untagged_target_adversary.rs`:

| Case | Asserts | Now |
|---|---|---|
| `a_tmp_holding_both_markers_does_not_qualify_an_untagged_target` | `target/` is kept when its only profile with both markers is `tmp/` | red |
| `a_tmp_holding_both_markers_does_not_qualify_the_parent_of_a_profile_git_reports` | the same, through the path where Git reports a profile on its own (cache.rs:153) | red |
| `a_bad_signature_tag_in_an_untagged_target_is_not_cargo_metadata` | a `CACHEDIR.TAG` with a bad signature is kept; only `target/debug` goes | red |
| `an_untagged_target_below_the_root_counts_only_beside_its_own_manifest` | `crates/a/target` goes, `crates/b/target` (its `Cargo.toml` is untracked) is kept | green |
| `case_variants_of_the_target_name_are_retained` | `Target/` and `TARGET/` are kept | green |
| `markers_that_are_regular_files_do_not_qualify_an_untagged_target` | `.fingerprint` or `deps` as a file does not qualify | green |
| `symlinks_inside_a_discarded_untagged_target_are_not_followed` | files behind symlinks in `deps`, in a profile and in `tmp` survive | green |
| `an_untagged_target_that_is_a_nested_repository_keeps_its_repository` | `target/.git` and its committed file survive | green |
| `a_tracked_file_inside_an_untagged_target_profile_survives` | a force-added file under `target/debug` survives | green |
| `a_target_real_cargo_built_after_mkdir_is_recognised` | with cargo 1.99.0, after `mkdir target/records` and `cargo build`: no tag, `target/debug` goes, `target/records` stays | green |

Red output:

```
---- a_tmp_holding_both_markers_does_not_qualify_an_untagged_target stdout ----
panicked at crates/worktree-cli/tests/untagged_target_adversary.rs:208:5:
{"applied":true,"discarded":[{"allocated_bytes":61440,"kind":"cargo-target","path":"target"}],...,"retained_ignored":[]}

---- a_tmp_holding_both_markers_does_not_qualify_the_parent_of_a_profile_git_reports stdout ----
panicked at crates/worktree-cli/tests/untagged_target_adversary.rs:226:5:
{"applied":true,"discarded":[{"allocated_bytes":20480,"kind":"cargo-profile","path":"target/release"},{"allocated_bytes":32768,"kind":"cargo-profile","path":"target/tmp"}],...,"retained_ignored":[]}

---- a_bad_signature_tag_in_an_untagged_target_is_not_cargo_metadata stdout ----
panicked at crates/worktree-cli/tests/untagged_target_adversary.rs:250:5:
assertion `left == right` failed: {"applied":true,"discarded":[{"allocated_bytes":36864,"kind":"cargo-target","path":"target"}],...}
  left: ["target"]
 right: ["target/debug"]
```

Mutant: `cache.rs:194` changed to check only the root `Cargo.toml` passes all 19 `cache_discard` tests; `an_untagged_target_below_the_root_counts_only_beside_its_own_manifest` catches it.

Origin: both `tmp` cases are green at base 78a5bdd; the bad-signature case is red at base only because base keeps the whole target, and base does not delete the tag.

Suite: `cargo test -p b10x-worktree-cli --locked --no-fail-fast` → 14 binaries ok (104), `untagged_target_adversary` 7 passed, 3 failed, EXIT=101.

| # | file:line | Verdict | Origin | Finding | What reaches it |
|---|---|---|---|---|---|
| 1 | crates/worktree-git/src/cache.rs:198 | INFEASIBLE | introduced | the qualifying `.any(full_cargo_profile)` counts `tmp/`; a target whose only full profile is `tmp/` qualifies and is deleted | discard-cache, finish --discard-cache, sweep; nothing found creates `target/tmp/.fingerprint` |
| 2 | crates/worktree-git/src/cache.rs:163 | INFEASIBLE | introduced | an untagged target is passed `tagged = true`, so a bad-signature `CACHEDIR.TAG` counts as Cargo metadata and goes with the target | same commands; nothing found writes a bad-signature tag |
| 3 | crates/worktree-git/src/cache.rs:194 | CONFIRMED | introduced | a root-only manifest mutant survives the unit's 19 tests | the unit's suite |

Attacked and not broken: case variants of `target`; markers as files; symlinks inside a discarded target; `target/` as a nested repository; a tracked file in a profile; real cargo 1.99.0 output; Git reporting a deeper path; a symlinked parent (reasoned); unusual `Cargo.toml` states (reasoned); a target below a target.

Written outside the worktree: `~/.cache/sccache` (sccache's own cache; home path shortened for the privacy scan).

```findings
- file: crates/worktree-git/src/cache.rs
  line: 198
  category: acceptance
  severity: warning
  verdict: INFEASIBLE
  origin: introduced
  message: the untagged-target qualifier counts tmp/ as a profile holding both markers, so a target with no real full profile is discarded, contradicting the contract and CARGO_TARGET_TMP's own invariant
- file: crates/worktree-git/src/cache.rs
  line: 163
  category: contract-drift
  severity: note
  verdict: INFEASIBLE
  origin: introduced
  message: an untagged target is classified with tagged=true, so the CACHEDIR.TAG metadata exemption deletes a bad-signature tag file Cargo never wrote
- file: crates/worktree-git/src/cache.rs
  line: 194
  category: mutant
  severity: note
  verdict: CONFIRMED
  origin: introduced
  message: checking only the root Cargo.toml for every untagged target survives all 19 cache_discard tests; only a target below the root distinguishes it
```
