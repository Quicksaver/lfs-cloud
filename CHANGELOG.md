# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- [added]: Add arrow-key configuration menus that infer repository IDs and select existing providers and removals.
- [added]: Automate isolated Google Drive authorization from a Desktop OAuth client JSON.
- [changed]: Let empty configurations use all server defaults without requiring a `server: {}` placeholder.

## [0.2.2] - 2026-08-05

- [added]: Generate durable-session encryption keys in the native credential store and add confirmed `lfscloud sessions generate-key` rotation that invalidates current sessions.
- [changed]: Hold an exclusive lifecycle lock for each metadata database so only one server can retain its sessions in memory and key rotation refuses to run while that server is active.
- [changed]: Listen on `0.0.0.0:15370` by default and infer Git LFS action URLs from each accepted connection, while retaining `server.public_url` for explicit hostname and proxy overrides.
- [changed]: Default GitHub repository providers to `https://api.github.com` so `api_url` is needed only for GitHub Enterprise or another override.
- [fixed]: Let configuration commands clear a stored GitHub API override and keep managed session-key bytes zeroed after use.
- [documentation]: Explain the credential exposure of plaintext LAN HTTP and the encrypted-tunnel but application-HTTP tradeoff for direct Tailscale access.

## [0.2.1] - 2026-08-04

- [changed]: Load `lfscloud.yml` from the user's home directory by default instead of depending on the command's working directory.
- [fixed]: Prevent targeted Windows draft verification from failing under PowerShell StrictMode when exactly one release is selected.

## [0.2.0] - 2026-08-03

- [added]: Preserve the legacy remote LFS URL in `.lfsconfig` so pulling the new target configuration does not prevent users from migrating remaining objects.
- [changed]: Authorize each user's GitHub identity and current repository permissions instead of sharing one server-configured GitHub account.
- [changed]: Reconcile and upload migrations through LFS Cloud so clients no longer need private server configuration or direct Google Drive access.
- [fixed]: Prevent fleet Windows verification from failing under SSH when Cargo, compiler, or nested smoke commands encounter rustup proxy links.
- [fixed]: Prevent Windows regression and smoke fixtures from failing before their intended assertions by consuming upload bodies and using current Basic action credentials.
- [fixed]: Let fleet Windows verification use the coordinator's active GitHub authentication without being rejected by an inaccessible inactive desktop credential.
- [fixed]: Reject stale migration config arguments, ignore the target as a legacy source, and validate server-issued upload actions before sending object bytes.
- [fixed]: Reject ambiguous fleet release resumes, surface remote validation failures, and repair missing Windows assets even after a green check.
- [fixed]: Validate the durable-session secret during config loading and count its documented minimum in characters.
- [documentation]: Group direct installers, Homebrew, WinGet, and checkout installation into compact README sections without advertising unavailable APT installation.
- [tooling]: Add one fail-fast fleet release command that fills missing Mac, Linux, and Windows checks, assembles the draft across both machines, and publishes the exact verified tag.
- [tooling]: Create the tagged draft before version verification so the native Windows continuation can start while macOS and Linux checks are still running.
- [tooling]: Save each aggregate verifier's output to its own retained log instead of terminal rolling windows, and purge verification logs older than 14 days.
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
