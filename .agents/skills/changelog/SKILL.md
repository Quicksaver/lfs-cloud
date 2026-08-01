---
description: Add or update an entry to the CHANGELOG.md file for the changes made.
name: changelog
---

Study `CHANGELOG.md`. Run a diff for the range provided; when a specific range is not provided, default to between the current branch and the main branch.

Add or update an entry under `## [Unreleased]` for the changes made:

- Be succinct and straight to the point
- Word the entry clearly for users
- Be explicit about the "symptom" that was addressed

Evaluate existing entries under `## [Unreleased]`, consider them implemented previously to the diff'd changes, and update them if the diff'd changes significantly altered their context or relevance.

Prefix related changes as `[prefix]: <entry>` with `added`, `changed`, `fixed`, `removed`, `deprecated`, `documentation`, `chore`, `tooling`. If a change does not fit any of these categories, create a new prefix that accurately describes the change. Group similarly prefixed entries together, and order them by the most recent changes first.

Adapt `README.md` if necessary to reflect any user-facing changes.
