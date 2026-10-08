---
format: aep.planning-md/3
id: story:release-notes-from-changelog
kind: story
status: active
title: The GitHub Release body is the version's CHANGELOG section
revision: 3
transitions:
- {from: "draft", to: "proposed", at: "2026-10-08T21:11:58Z", actor: "human:timo", revision: 2}
- {from: "proposed", to: "active", at: "2026-10-08T21:11:58Z", actor: "human:timo", revision: 3}
---
## Outcome

A release's GitHub Release body is that version's `CHANGELOG.md` section, not the tag message.

## Why

The 0.13.0 Release body was only "0.13.0" (`gh release view 0.13.0`, 2026-10-08): the workflow
copies the annotated tag's message, and the changes are only in `CHANGELOG.md` at the tag.

## Contract

- `.github/workflows/release.yml`, job `publish`: the notes are the lines after the dated heading
  `## <version> — <date>` up to the next `## ` heading. A tag whose section is missing or empty
  fails the job (the gate job already requires the heading).
- A Release that already exists keeps its notes, as before.

## Acceptance

- The next release's body equals its `CHANGELOG.md` section (`gh release view <tag> --json body`).
- `actionlint`-free YAML: the workflow parses (`gh workflow view release.yml` after the push).
