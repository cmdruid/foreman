# Mock Project Runbook

1. Read the worker assignment from the provided prompt.
2. Confirm an explicit `WORKTREE_PATH=<path>` is present. If missing, create:
   - `<project-root>/.audit-worktrees/<category>-<timestamp>`
3. Create the worker worktree:
   - `git -C "<project-root>" worktree add -b "<worker-branch>" "$WORKTREE_PATH" "<base-branch>"`
4. Read the target issue prompt file at:
   - `tasks/<category>.md`
5. Produce one concrete deliverable file in the worktree:
   - `.audit-generated/<category>-worker-deliverable.md`
6. Return a concise final summary in `final_text`.
7. Do not edit or alter root project files from the worker scope.
