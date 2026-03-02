# Contrib Assets

This folder contains optional assets for tooling integrations.

- `codex-foreman-skill/`
  - Reusable skill package for Codex/OpenClaw to use `codex-foreman`.
  - Copy or install this folder into your runtime skill directory as `codex-foreman`.
- `mock/`
  - Self-contained mock project with explicit worktree-based worker instructions.
- `run_mock_demo.sh`
  - End-to-end executable script that dispatches real worker jobs and verifies real
    deliverables in worktrees.
- `run_mock_mixed_demo.sh`
  - End-to-end executable script that dispatches a mixed job list (worktree-backed
    and non-worktree workers) in a single `create_project_jobs` call.
