# Mock Project

This fixture is a lightweight, deterministic scenario for exercising
`foreman` with real workers and **real worktree deliverables**.

- Foreman and worker prompts are explicit.
- Workers are assigned clear categories and target paths.
- Deliverables are concrete files written in each worker's output path.

Use with:

- `contrib/demo/run_demo.sh` (all workers with worktrees)
- `contrib/demo/run_mixed.sh` (mixed worktree and non-worktree worker contracts)

You can also force mixed mode directly:

- `RUN_MOCK_DEMO_MODE=mixed ./contrib/demo/run_demo.sh`
