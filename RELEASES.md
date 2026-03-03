# Releases

## Unreleased

- Added configurable template loading for project scaffolds:
  - default scaffold files now come from `templates/`
  - new `--template-dir PATH` flag and `CODEX_FOREMAN_TEMPLATE_DIR` env override
  - runtime fallback search path now includes:
    - CLI/env override
    - `$(CARGO_MANIFEST_DIR)/templates`
    - executable-adjacent `templates`
    - `/usr/share/foreman/templates`
    - `/etc/foreman/templates`
- Added callback profile validation with `--validate-config`.
- Added callback-only send flow (`POST /agents/:id/send` with no `prompt` and callback overrides only).
- Made `SendAgentInput.prompt` optional to support callback steering updates independent of turn starts.
- Added projects v1 API:
  - `POST /projects`
  - `GET /projects`
  - `GET /projects/:id`
  - `DELETE /projects/:id`
  - `POST /projects/:id/workers`
  - `POST /projects/:id/foreman/send`
  - `POST /projects/:id/foreman/steer`
  - `POST /projects/:id/compact`
- Added project config model and runtime files:
  - `project.toml` parsing and prompt loading
  - `FOREMAN.md`, `WORKER.md`, `RUNBOOK.md`, `HANDOFF.md` integration
- Added project event callbacks and lifecycle callback channels:
  - `start`, `compact`, `stop`, `worker_completed`, `worker_aborted`
  - `policy.compact_after_turns` and `policy.bubble_up_events`
- Extended callback event payload and `prompt` templating support for better external-agent forwarding.
- Added hard-cut app-server lifecycle management: local stdio `codex app-server` launch and monitoring only.
- Added durable state persistence:
  - persisted state at `--state-path`
  - startup recovery of agents/projects with callback/runtime metadata
- Added and updated documentation set:
  - `docs/manual.md`
  - `CONTRIBUTING.md`
  - `TESTING.md`
- `--init-project` project scaffold generator for `FOREMAN.md`, `WORKER.md`, `RUNBOOK.md`, `HANDOFF.md`, and `project.toml`
- Switched app-server transport to a hard-cut, single-mode stdio process model. `foreman` now always launches/monitors a local `codex app-server` child.
- Expanded README for project scopes and service validation.

### Release Readiness Checklist

- [x] API contract and behavior coverage
  - `POST /agents`
  - `GET /agents`, `GET /agents/:id`, `GET /agents/:id/result`, `GET /agents/:id/wait`, `GET /agents/:id/events`, `POST /agents/:id/send`, `POST /agents/:id/steer`, `POST /agents/:id/interrupt`, `DELETE /agents/:id`
  - `POST /projects`, `GET /projects`, `GET /projects/:id`, `GET /projects/:id/callback-status`, `DELETE /projects/:id`, `POST /projects/:id/workers`, `POST /projects/:id/foreman/send`, `POST /projects/:id/foreman/steer`, `POST /projects/:id/compact`
- [x] Status endpoint coverage
  - `GET /status`
- [x] Mocking and deterministic event fixture support
  - `test/bin/fake_codex.rs` for stable JSON-RPC behavior.
- webhook capture endpoint in `test/common`.
- [x] Test artifacts
- Fixture suites in `test/fixtures/project-valid`, `test/fixtures/project-missing-worker`, `test/fixtures/project-invalid-config`.
- [x] Automated test suites
- Integration tests: `test/integration.rs`
- End-to-end tests: `test/e2e.rs`
- [x] Documentation set
  - `README.md`
  - `docs/manual.md`
  - `TESTING.md`
  - `CONTRIBUTING.md`
  - `CHANGELOG.md`
- [x] Security hardening (API token auth + startup validation).
- [x] CI pipeline with pinned toolchain and test matrix.
- [x] Versioning + release notes verification for changelog consistency.
- [x] Failure-injection tests for callback timeouts and process crash recovery.
- [x] Tag-triggered GitHub release workflow in `.github/workflows/release.yml`:
  - Linux/macOS `.tar.gz` and Windows `.zip` builds
  - changelog-driven release notes extraction
  - checksum generation and GitHub release attachment
- [x] GitHub Actions pinned to immutable SHAs in release and CI workflows:
  - `actions/checkout`
  - `dtolnay/rust-toolchain`
  - `actions/upload-artifact`
  - `actions/download-artifact`
  - `softprops/action-gh-release`
- [x] Workflow hardening checks in CI:
  - New `validate-workflows` job runs `scripts/verify_action_shas.sh`
  - Script enforces that every workflow `uses:` action ref is a full 40-char commit SHA

### Shipping Binaries With Templates

Release artifacts must include the template assets because `--init-project` reads templates from disk at runtime.

Recommended install layout:
- `/usr/local/bin/foreman`
- `/usr/share/foreman/templates/project.toml`
- `/usr/share/foreman/templates/FOREMAN.md`
- `/usr/share/foreman/templates/WORKER.md`
- `/usr/share/foreman/templates/RUNBOOK.md`
- `/usr/share/foreman/templates/HANDOFF.md`
- `/usr/share/foreman/templates/MANUAL.md`

Release commands:

```bash
cargo build --release
install -Dm755 target/release/foreman /usr/local/bin/foreman
install -d /usr/share/foreman/templates
cp -r templates/*.md templates/*.toml /usr/share/foreman/templates/
chown -R root:root /usr/local/bin/foreman /usr/share/foreman/templates
```

Runtime invocation options:
- CLI: `--template-dir /usr/share/foreman/templates`
- Environment: `CODEX_FOREMAN_TEMPLATE_DIR=/usr/share/foreman/templates`

### Automated GitHub Release (Release Tag)

For each pushed tag matching `v*`, the release workflow:

1. Builds artifacts for Linux, macOS, and Windows in parallel.
2. Bundles:
   - `foreman` binary (from `target/<platform>/release`)
   - `templates/` directory
3. Generates per-platform archive checksum files (`*.sha256`).
4. Extracts release notes from `CHANGELOG.md`:
   - prefers `## v<version>` for the current tag
   - falls back to `## Unreleased`
5. Publishes a GitHub release attaching:
   - `dist/foreman-*.tar.gz`
   - `dist/foreman-*.tar.gz.sha256`
   - `dist/foreman-*.zip`
   - `dist/foreman-*.zip.sha256`
6. Uses immutable action revisions and enforces release pipeline protections:
    - `workflow_dispatch` for manual one-off runs
   - manual tag input validation (`RELEASE_TAG`) with strict semver pattern `vX.Y.Z` (optional prerelease suffix)
   - release tag consistency propagated through both build/publish stages
    - serialized/concurrency-safe release execution
    - short-lived uploaded artifacts (14-day retention) in GitHub Actions

To publish:

```bash
git tag -a v0.2.0 -m "Release v0.2.0"
git push origin v0.2.0
```

Before tagging, run the release gate commands in `TESTING.md` and verify:

- all unit/integration/e2e tests pass in CI
- mixed-mode mock demo ends in `result: success`
- release metadata test passes (package version, changelog, and releases entries)

For a consolidated checklist, run:

```bash
cat RELEASE.md
```

## v0.1.0

- Initial MVP with:
  - worker lifecycle API (`spawn`, `send`, `steer`, `interrupt`, `close`)
  - basic webhook callback support
  - Codex `app-server` transport integration
