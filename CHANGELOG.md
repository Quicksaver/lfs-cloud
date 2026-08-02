# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- [documentation]: Group direct installers, Homebrew, WinGet, and checkout installation into compact README sections without advertising unavailable APT installation.
- [tooling]: Generate required WinGet schema headers and update existing submission branches so requested manifest changes can be addressed.
- [tooling]: Resume failed Homebrew publication by trusting the configured tap and accepting its matching generated formula, and prevent duplicate WinGet upstream remotes.
- [tooling]: Publish a selected verified release without requiring a redundant typed `publish vX.Y.Z` confirmation.
- [tooling]: Skip Cloudsmith and APT distribution when no target is configured instead of blocking otherwise publishable releases.
- [tooling]: Keep publisher preflight failures visible after interactive release selection instead of clearing them with the terminal live region.
- [tooling]: Preserve trusted commit-status provenance when selecting publishable drafts so fully green releases are not omitted.
- [tooling]: Run final cross-platform release publication natively from macOS with Bash instead of requiring PowerShell or a clean worktree.

## [0.1.4] - 2026-08-01

- [tooling]: Repeated local release commands now resume untagged version commits or reject an already-tagged `HEAD`, while failed or unpublished release notes roll forward until a version is published.

## [0.1.3] - 2026-08-01

Version bump only.

## [0.1.2] - 2026-08-01

- [fixed]: Prevent both Linux verifiers from writing Debian packages to a nonexistent `/dist` directory inside their reusable containers.
- [tooling]: Prevent persistent Linux verification target volumes from retaining several gigabytes of one-run Cargo incremental artifacts.
- [tooling]: Add checksummed direct-install scripts for macOS, Linux, and Windows, plus Homebrew, WinGet, and Debian package publication.
- [tooling]: Assemble and verify every platform asset in a GitHub draft, then publish it through a local interactive command as an immutable release with resumable distribution statuses.
- [tooling]: Releases now roll current changelog entries into a dated version section and use that section as the GitHub release description.
