---
format: aep.planning-md/3
id: review-result:trial-review-design-r1
kind: review-result
status: active
title: Design critic, downstream trial-review set, round 1
relations:
- reviews: story:a-nested-keep-file-keeps-its-directory
revision: 1
---
needs-revision
story:a-nested-keep-file-keeps-its-directory — it extends the keep rule for `discard-cache`, `sweep`, `finish` and `archive`, but not for the other reader of that rule. `prune-archives --strip-build-output` (from story:strip-ignored-output-from-archives) reads only the root keep file. An archive's nested-kept paths under a directory the root `.gitignore` ignores could then be stripped later — .engineering/planning/story/a-nested-keep-file-keeps-its-directory.md:25, crates/worktree-domain/src/build_output.rs:568
Read: 1 artifact and the two keep-file stories already in the store. Commands: `validate` (valid), and a grep for `KEEP_FILE` readers. The story has no edges, so there is no cycle.
Not established (outside my lane): the story carries no parent or serves relation.
```findings
- {file: .engineering/planning/story/a-nested-keep-file-keeps-its-directory.md, line: 25, category: design, severity: warning, verdict: needs-revision, origin: introduced, message: "it extends the keep rule for discard-cache, sweep, finish and archive but not for prune-archives --strip-build-output, which reads only the root keep file, so nested-kept paths can later be stripped from archives"}
```
