# Changelog

All notable changes to `foreman` are documented here.

## Unreleased

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
