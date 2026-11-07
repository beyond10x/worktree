---
format: aep.planning-md/3
id: story:recovery-proof-in-bounded-git-calls
kind: story
status: active
title: Recovery proof costs a bounded number of Git calls per remote, and gc shares one advertisement per assessment
summary: A removal proof ran two Git processes per advertised ref, twice, and two ls-remote calls per tree; gc --apply refused a whole run after one reviewed id stopped being a candidate. Batch the per-ref questions, read a remote once unless a fetch happened, share the advertisement across an assessment, and retain the one id instead of refusing the run.
scope:
- confidence: cited
  path: crates/worktree-git/src/lib.rs
- confidence: cited
  path: crates/worktree/src/lib.rs
revision: 4
transitions:
- {from: "draft", to: "proposed", at: "2026-11-07T04:41:06Z", actor: "human:timo", revision: 2}
- {from: "proposed", to: "active", at: "2026-11-07T04:41:06Z", actor: "human:timo", revision: 3}
---
# Story: recovery proof in bounded Git calls

## Problem (observed 2026-11-07)

- `strace -f -e execve` of `worktree gc --dry-run --id <one tree>` against `beyond10x/rfcspec`
  (18 advertised refs): 87 Git processes, 3.6 s wall; 37 `cat-file -e`, 36 `merge-base
  --is-ancestor`, 2 `ls-remote` (2.87 s of the 3.6 s).
- The per-ref calls scale with the advertisement: a repository advertising 949 refs makes one
  proof there is about 3,800 processes, and `gc --apply` proves each tree twice.
- `gc --scope profile --dry-run` over a profile of 468 records took 2,448 s; over one of
  127 records 1,000 s; one of about 1,170 live records had
  not finished after 85 minutes.
- A `gc --apply` over 403 reviewed `b10x` ids ran 34 minutes, then refused the whole run with
  `selected-worktree-not-cleanup-candidate` because one id (`takeover`) had been removed by
  another session meanwhile. Nothing was removed.

## Change

- `ProcessGit::present_commits`: one `cat-file --batch-check` over every advertised object
  (`<object>^{commit}` lines) says which are commits this repository holds, and their peeled ids.
- `ProcessGit::commits_containing`: one `rev-list --parents --stdin` (`<candidate>` lines and
  `^<head>`) lists every commit reachable from a candidate that is not an ancestor of HEAD; a
  candidate contains HEAD when it is HEAD or a walk up the listed children from HEAD reaches it.
  Replacement objects and graft ancestry stay disabled, as for `merge-base --is-ancestor`.
- `observe_recovery` reads a remote once, and again only after it fetched objects from it.
- `GitPort::begin_shared_observation` / `end_shared_observation` (default: share nothing).
  `ProcessGit` keeps one advertisement per `(repository, remote)` while sharing is on, replaced by
  the advertisement read after a fetch. `gc_selected` shares across its assessment of every
  candidate and ends sharing before `apply_cleanup`, so the proof immediately before a removal is
  read afresh.
- `gc_selected` reports a reviewed id that is no longer a candidate as a retained assessment with
  `selected-worktree-not-cleanup-candidate` and applies the other reviewed ids.

## Acceptance

- `present_commits` peels an annotated tag to its commit and leaves out a blob and an unknown id.
- `commits_containing` names HEAD itself and its descendants among the candidates, and nothing
  else; a grafted ancestry still does not count.
- With sharing on, a ref pushed after the first observation is not seen until sharing ends; a
  fetch during sharing replaces the shared advertisement.
- The façade shares during a dry-run (`begin`, proof, `end`) and proves afresh after `end` on
  apply (`begin`, proof, `end`, proof).
- `gc --apply` over a candidate and a record removed meanwhile removes the candidate and retains
  the other with `selected-worktree-not-cleanup-candidate`.
- `task check` is green.
