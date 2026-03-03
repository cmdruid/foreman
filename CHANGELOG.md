# Changelog

All notable changes to `foreman` are documented here.

## Unreleased

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
