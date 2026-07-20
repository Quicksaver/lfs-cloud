---
name: smoke-test
description: Run the complete LFS Cloud smoke suite across the CLI, disposable Git repositories, local cache, migration, credentials, server behavior, and optional live providers. Use after implementation work, before release, or when asked to smoke test the project.
---

# Smoke Test

Run the suite once from the repository root in a single tool call:

```bash
node --no-warnings --experimental-strip-types .agents/skills/smoke-test/scripts/smoke-test.ts
```

Follow these rules:

- Use the named `PASS`, `FAIL`, and `SKIP` results as the report.
- Let the runner finish all tests after a failure.
- Load credentials from root `.env.local`; explicit environment variables take precedence.
- Keep `~/Sites/throwaway` itself unchanged. The runner creates and removes one isolated child directory there.
- Use disposable repositories for `init` and migration. They isolate remotes, config, and history better than linked worktrees.
- Leave general temporary-directory tests in the system temp location so they do not inherit `throwaway` Git state.
- Create linked worktrees only for behavior that specifically depends on multiple worktrees.
- Enable GitHub and Drive checks when their credentials are present. Enable LAN checks with their explicit flag and config.
- Report every failure and the final summary. Do not rerun individual tests unless diagnosing a failure.
