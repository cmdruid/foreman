# Contrib Assets

This folder contains optional assets for tooling integrations.

- `foreman-skill/`
  - Reusable skill package for Codex/OpenClaw to use `foreman`.
  - Copy or install this folder into your runtime skill directory as `foreman`.
- `demo/`
  - Self-contained mock project with explicit worktree-based worker instructions.
- `demo/run_demo.sh`
  - End-to-end executable script that dispatches real worker jobs and verifies
    real deliverables (worktree mode by default).
- `RUN_MOCK_DEMO_MODE=worktree` / `RUN_MOCK_DEMO_MODE=mixed`:
  - `worktree`: all workers require dedicated worktrees.
  - `mixed`: one non-worktree and three worktree workers.
- `demo/run_mixed.sh`
  - Wrapper script that sets `RUN_MOCK_DEMO_MODE=mixed`.
