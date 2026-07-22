---
name: fix-ci
description: Debug and fix the latest LFS Cloud CI failure, push the fix, and monitor replacement builds until green. Use when LFS Cloud GitHub Actions fails or the user asks to resolve the latest CI or build alert end to end.
---

# Fix CI

1. Use `$read-lfs-cloud-slack` to read the latest LFS Cloud Actions failure.
2. Debug and fix the latest failure.
3. Push the fix and monitor the next build.
4. If the build fails, repeat until it is green.
