# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- [fixed]: Prevent persistent Linux verification target volumes from retaining several gigabytes of one-run Cargo incremental artifacts.
- [added]: Add checksummed direct-install scripts for macOS, Linux, and Windows, plus Homebrew, WinGet, and Debian package publication.
- [changed]: Assemble and verify every platform asset in a GitHub draft, then publish it through a local interactive command as an immutable release with resumable distribution statuses.
- [changed]: Releases now roll current changelog entries into a dated version section and use that section as the GitHub release description.
