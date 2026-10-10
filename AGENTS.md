# AGENTS.md — worktree

## Serves

- **O2 — decisions as data, with evidence.** Worktree placement, ownership, recoverability and
  cleanup are typed decisions with recorded proof.
- **O6 — self-improvement, built into all of it.** Agent work leaves bounded, inspectable state and
  a deterministic cleanup path instead of accumulating invisible machine debris.

## Boundary

This repository owns safe Git worktree lifecycle as a reusable Rust library and CLI. It knows no
Atlas, agent harness, plugin marketplace or organization repository inventory. The `worktree`
agent plugin lives in `beyond10x/agentplugins`. Consumers supply profiles and adapters from above.

The `b10x-worktree-domain` crate performs no I/O. `b10x-worktree` owns the application ports and
orchestration. Concrete Git and SQLite behavior stays in their adapter crates. The CLI contains no
independent policy: every decision must come from the public façade.

## Invariants

- Never invoke a command through a shell. Git arguments are discrete argv values.
- Never remove a worktree with `--force`.
- Every removal requires an exact linked-worktree member whose HEAD remains stable across the final
  proof and intent observations, no tracked, untracked, or ignored state, no live lease or Git
  operational/worktree lock or in-progress Git operation, and fresh proof from exact refs currently
  advertised by a configured remote: either the exact HEAD is an ancestor of one, or every commit
  no advertised ref holds is a single-parent, non-empty commit whose whitespace-exact patch one
  advertised ref carries. Patch equivalence never weakens to Git's whitespace-insensitive patch ids
  alone.
- Every removal also refuses state Git status does not report: assume-unchanged and skip-worktree
  entries, staged content that differs from both HEAD and the working copy, any `.git` below the
  tree's root, and per-worktree refs. No proof, remote or archived, covers it, with one exception:
  a `.git` at or below a nested repository root that the archive the removal relies on images,
  re-verified against the tree in the same flow.
- The one substitute for that remote proof is a verified local archive of the same record and
  HEAD: its files keep their recorded SHA-256, the bundle's own pack holds every object HEAD adds
  over freshly advertised refs, and the tree's complete on-disk content still has the archived
  fingerprint, whether or not Git reports it dirty; remote refs never cover uncommitted state. Such
  a tree is returned to HEAD by resetting the index, then restoring or deleting each file only after
  re-hashing it against the archive, before the non-forced removal. Archiving never modifies the
  tree, and only `prune-archives --apply` deletes an archive, and only under its rule.
- Cargo build layout is the one content an archive leaves out. Below a cargo target that
  `discard-cache`'s structural rule recognises, a file neither HEAD nor the index tracks is build
  layout when its first component below the target is `debug`, `release`, `tmp`, `CACHEDIR.TAG`,
  `.rustc_info.json` or a profile directory (holding `.fingerprint/`; also
  `<target>/<triple>/<profile>/`); everything else below the target stays archived. Such an
  archive is `worktree.archive/3` and records per target the files and bytes left out; one that
  left nothing out is written exactly as format 1 or 2. Its fingerprint excludes that layout, and
  layout below a target it did not record refuses as `archive-stale`. Removal through it deletes
  each left-out file only after re-observing it, immediately before deletion, as a file or
  symlink of build layout below the same recognised target; anything else refuses.
- `prune-archives --strip-build-output` never deletes an archive. A dry-run, the default, writes
  nothing. `--apply` takes exact `--id` values only, re-assesses each, and rewrites only an archive
  whose manifest is valid, whose directory holds only recorded files, whose tree path is absent and
  whose repository exists. It streams `dirty.patch` (verifying its recorded digest) and removes only
  whole `diff --git` sections that add a file (`new file mode`) whose decoded path is build layout
  below a directory the patch's own added files recognise as a target (`<t>/CACHEDIR.TAG`,
  `<t>/.rustc_info.json` or `<t>/<p>/.fingerprint/…`); an undecodable path keeps its section, and
  nested images are never touched. The new patch is written outside the archive, verified to apply
  over HEAD in a scratch index (never a tree's or the repository's own index), `worktree_tree` is
  recomputed from it, and only then are the patch replaced (or deleted when nothing remains) and
  the format 3 manifest written last. Any refusal leaves the archive byte for byte as it was; an
  interruption between the two replacements leaves the old manifest naming a patch that no longer
  has its digest, which verification and pruning refuse.
- `prune-archives --apply` deletes only archive directories named by exact `--id`, each
  re-assessed immediately before deletion and deleted only when `Removable`: a valid manifest,
  nothing in the directory the manifest does not name as a regular file, the registered tree path
  absent, no nested repository image, no patch, and HEAD and every unique commit an ancestor of a
  ref freshly advertised by a configured remote of the manifest's repository (no local ref,
  replacement or graft counts; patch equivalence does not count). Offline, a missing repository or
  no remote refuses. It deletes the files the manifest names, then the manifest, then the empty
  directory without recursion. No flag, environment variable or configuration makes a refused
  archive removable, and files below the archive root that are not archive directories are never
  deleted.
- A nested repository (Git lists it only as `<path>/`, and its `.git` is a real directory) is
  archived as a byte image, `nested-<n>.tar`, whose canonical listing's SHA-256 is recorded; the
  image read back, and the root on disk before and after writing, must give that fingerprint.
  Removal deletes an imaged root bottom-up, re-observing each entry against the image's listing
  immediately before deleting it. A gitfile or symlinked `.git`, `commondir`, non-empty
  `objects/info/alternates`, `worktrees/` entries or a symlink inside any nested `.git`, a root at
  or below which HEAD or the index tracks anything, and any entry that is not a file, directory or
  symlink are refused, never imaged.
- The only command that deletes files a tree's owner wrote is `discard-cache` (and `finish
  --discard-cache`), and it deletes only ignored, real directories recognised as build cache by
  structure: a Cargo profile holding `.fingerprint/` inside a validly tagged target, that target's
  own real `tmp/` (Cargo's `CARGO_TARGET_TMPDIR`, whatever it holds) when the target holds a
  profile, `node_modules` at or below a tracked lockfile, a virtual environment beside a tracked
  Python manifest, and a tagged `.pytest_cache`, `.mypy_cache` or `.ruff_cache`. Cargo tags only a
  target it creates, so a real directory named `target` without a valid tag counts as tagged when
  a tracked `Cargo.toml` sits beside it and a direct child other than `tmp/` holds both
  `.fingerprint/` and `deps/` as real directories. A name alone never qualifies; anything
  unrecognised is retained and reported. It refuses for a live lease, a Git lock, or another
  process using the tree. The one other deletion of such files is removal through a
  `worktree.archive/3` archive, and only of the build layout it left out, re-observed as above.
- `sweep` composes only `discard-cache` and `archive`. It never changes lifecycle, never removes a
  tree and never applies GC; removal stays an exact-id, reviewed `gc --apply`.
- Ordinary GC requires canonical containment below the configured worktree root. Only exact-id
  reconciliation with separate external-retirement confirmation may retire a finished external
  legacy tree after the same removal gates pass.
- Never clear stale relocation state by hand. A finished external legacy tree may supersede a
  pre-0.3 relocation intent only when the exact source and all recorded HEADs agree and the
  destination is absent from both Git and the filesystem; ambiguous state remains a refusal.
- Offline or ambiguous recovery evidence is a refusal, never permission to delete.
- Forgetting a record is not recovering its work. A missing record whose commit no ref holds may be
  abandoned only through a reviewed exact-id apply in which the operator names that exact commit,
  and only while Git still confirms nothing points at it. When the record's repository is gone
  (its root absent, or without `.git`), that acknowledgement alone retires it, and only once every
  path the record and its intents name is absent; an existing tree is refused and never touched.
- Local replacement refs, graft files, and inherited graft configuration must never influence
  remote recovery proof.
- Create plans use immutable commits and canonical policy-derived paths; plans and exact repository
  membership are revalidated immediately before mutation.
- Apply operations for GC and reconciliation require exact reviewed worktree ids. Cleanup and
  relocation lifecycle claims, final HEAD updates, and lease exclusions are atomic, and
  proof-bearing removal intent is durable.
- CLI JSON protocol version 4, reconciliation version 4, inspection format
  `worktree.inspection/2`, archive manifest formats `worktree.archive/1` (no nested repository),
  `worktree.archive/2` (adds `nested_repositories`) and `worktree.archive/3` (adds
  `build_output`), hook protocol version 1,
  and configuration/workspace-policy version 1 are immutable after release. Cut a new surface
  version for a wire change.
- Generated skill content comes from `worktree skill`; do not edit it by hand.
- A public API belongs in a library crate. The binary is an adapter, not the product boundary.

## Gate

```console
task check
```

Anything executable in this repository is Rust. Releases use bare SemVer tags from `main`, after
`CHANGELOG.md`, every workspace package version and `Cargo.lock` agree. Pushing the tag runs
`.github/workflows/release.yml`, which builds `worktree-<version>-<target>.tar.gz` for four targets,
writes `SHA256SUMS`, and publishes them on the GitHub Release; a release is complete when those five
assets are there.

<!-- b10x-docs-operations:start -->
## Public documentation operations

This repository owns the public source and presentation allowlist in `b10x.docs.yaml`. The generated credential-free `.github/workflows/b10x-docs-bundle.yml` passively packages only those declared files for the exact successful `main` commit; it must never run repository code. The generated `.github/workflows/b10x-docs-check.yml` runs the publisher's per-source checks on every pull request and main push, with read-only contents and no credentials; it is deliberately separate from the shared gate, which runs on `pull_request_target` with a secret and never reads candidate source. Atlas selects the latest successful bundle with every other catalog source, and Website plus Docs System own rendering, shared components, search, and feeds. Do not add a standalone docs deployer or put App credentials in this public repository. If Atlas catalogs a former Pages workflow, that file remains repository-owned validation: preserve its bespoke checks while keeping exact read-only permissions, an unconditional pull-request trigger, and no deployment primitives. Project Pages at `/worktree/` is only the generated stable redirect façade in `.github/workflows/b10x-docs-pages.yml`; content-only publication never rebuilds it.

From the complete organization workspace, verify the contract with a clean Atlas checkout at the current remote `main`. Set `B10X_ATLAS_CHECKOUT` to a managed Atlas worktree when the primary checkout is dirty or stale; never infer command availability from the primary alone.

```bash
atlas_checkout="${B10X_ATLAS_CHECKOUT:-atlas}"
atlas_head="$(git -C "$atlas_checkout" rev-parse HEAD)"
atlas_main="$(git -C "$atlas_checkout" ls-remote origin refs/heads/main | awk '{print $1}')"
test -z "$(git -C "$atlas_checkout" status --porcelain)"
test "$atlas_head" = "$atlas_main"
cargo run --manifest-path "$atlas_checkout/Cargo.toml" --locked -q -- \
  --store "$atlas_checkout/catalog/store" docs reconcile --workspace . --check
```

Keep internal plans, stories, ADRs, decisions, worklogs, security material, and research out of the public allowlist unless a repository authority explicitly declares them public.
<!-- b10x-docs-operations:end -->

<!-- b10x-release-operations:start -->
## Release completion

An ordinary release completes after this repository's exact tag, required source checks,
published release and required artifacts are verified. A pushed tag with unfinished checks or
uploads is queued; report it as released only after those requirements succeed.

Atlas reconciliation and public documentation publication run asynchronously. Do not wait for
Atlas or Website, update Website source locks or bootstrap snapshots, promote consumer pins,
release shared docs tooling, or redeploy documentation façades as part of an ordinary source
release. Report documentation as pending unless its publication was actually verified. A background
documentation failure does not invalidate a successful source release.

Keep this repository's provenance, correctness, security, compatibility and artifact verification
requirements. Shared rendering, routing or delivery-control changes still require their relevant
integration gates. A release request does not authorize deployment or downstream releases.
Repositories without a release unit retain their existing publication policy. This completion
boundary supersedes older instructions that attach synchronous documentation ceremony to each
source release.
<!-- b10x-release-operations:end -->
