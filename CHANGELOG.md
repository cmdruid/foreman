# Changelog

All notable changes to `foreman` are documented here.

## Unreleased

## [0.4.1]

- Added `POST /projects/:id/reload` to hot-reload project config from `project.toml`, with in-place updates for callback configuration, validation, policy, and `jobs.defaults.{min_turns,strategy}`.
- Added `Foreman::reload_project_config` with explicit safe/unsafe hot-reload boundaries and a regression test for missing-project error handling.
- Added command callback shell execution mode (`shell = true`) and automatic shell fallback for non-absolute command programs using `bash -lc`.
- Added project-level callback environment support via `callbacks.env`, merged below profile env and below system env precedence during command dispatch.
- Relaxed command callback profile validation to permit non-absolute programs when shell execution is used.

## [0.4.0]

- A1: Added native per-worker git worktree lifecycle management for project jobs, including automatic worktree branch/path generation, merge strategies (`sequential`, `rebase`, `manual`), merge conflict reporting, and cleanup on completion/abort/shutdown reconcile.
- A2: Added worker validation gates for `on_turn` and `on_complete` commands with configurable `fail_action` (`retry`, `abort`, `warn`) and retry budgeting.
- A3: Added structured multi-turn guardrails with `jobs.defaults.min_turns` plus `explore-plan-execute` strategy continuation prompts.
- B: Added tool-filtered callbacks (`callbacks.filters`) with tool + command pattern matching and optional debounce windows for scoped callback dispatch.

## [0.3.1]

- Fixed worker log/progress tool parsing to consume `item/completed` payloads using `params.item.type`, including `commandExecution`, `fileChange`, and `dynamicToolCall`.
- Fixed empty/partial worker log field extraction by reading command/path/exit-code/stderr/bytes from nested `params.item` values, including `fileChange` `changes[].path` handling.
- Fixed `files_modified` progress stats for `fileChange` items by collecting all changed paths from `params.item.changes`.
- Fixed project lifecycle callback dispatch to bypass generic event-filter matching during lifecycle dispatch, so configured lifecycle callbacks execute for their selected lifecycle key.

## [0.3.0]

- Added structured startup error UX with `run()` wrapping, hint suggestions, and `--force` lockfile override support for crash recovery.
- Added turn-level observability on `GET /agents/:id` with lazily derived progress stats (`turns_completed`, `last_tool_call`, `files_modified`, `elapsed_ms`) from buffered events.
- Added worker activity log extraction endpoint `GET /agents/:id/logs` with `tail`/`turn` filters and method+params parsing into structured log entries.
- Added unix domain socket callback support (`type = "unix-socket"` profiles plus project `socket` callback specs), dispatching newline-delimited JSON with timeout handling.
- Added live job event streaming endpoint `GET /jobs/:id/events` using SSE (`text/event-stream`) with `Last-Event-ID` replay and terminal `job.completed` event emission.

## [0.2.2]

- Added graceful shutdown handling for both `SIGINT` (`ctrl_c`) and `SIGTERM` so process-manager termination follows the same shutdown path.
- Reconciled agent statuses before final shutdown persist so completed agents are not left as `running`/`restarting` in saved state.
- Improved live PID lockfile conflict visibility by printing the conflict to stderr and returning a clearer actionable error that includes the socket path.

## [0.2.1]

- Fixed a deadlock in `restart_stalled_worker` by snapshotting worker results and dropping the `agents` write lock before job/project update calls that re-read agent state.
- Reduced notification log noise by silently ignoring threadless `account/rateLimits/updated` notifications.
- Hardened event-loop resiliency by increasing broadcast channel capacity to `1024` and continuing after broadcast lag events instead of terminating the loop.
- Added startup PID lockfile protection (`.foreman/foreman.lock`) to prevent dual-instance socket collisions, with stale lock takeover and lockfile cleanup on graceful shutdown.

## v0.2.0

- Hardened callback and lifecycle behavior in project orchestration, including clearer status handling for worker completion/abort paths.
- Improved project callback profile resolution and dispatch behavior between project-local and service-level profiles.
- Updated CLI/docs defaults and examples to match current `foreman` workflows and project-first usage.
- Expanded integration and e2e coverage for:
  - project foreman send/steer endpoints
  - job listing/wait/result behavior
  - auth header/token permutations
  - project-local and global callback fallback/override paths
  - worker-aborted lifecycle callback handling
- Added a release-consistency test that verifies release metadata tracks package version.
- Added `CHANGELOG.md` and expanded release validation guidance for official release quality.

## v0.1.0

- Hard-cut runtime architecture with local stdio `codex app-server` management.
- Added durable state persistence and restart recovery for agents and projects.
- Added callback support for webhook and command profiles with templating and event filters.
- Added project orchestration (`/projects`) with `FOREMAN.md`, `WORKER.md`, `RUNBOOK.md`, `HANDOFF.md` conventions.
- Added compaction, worker lifecycle controls, callback bubbling, and hook execution.
- Added official release quality artifacts and test scaffolding:
  - `docs/manual.md`
  - `CONTRIBUTING.md`
  - `TESTING.md`
  - `RELEASES.md`
  - `test/bin/fake_codex.rs`
  - integration and e2e test coverage
