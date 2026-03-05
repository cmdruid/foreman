# Changelog

All notable changes to `foreman` are documented here.

## Unreleased

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
