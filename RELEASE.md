# foreman release guide

This file is the single source of truth for release process.
Versioned release notes live in `CHANGELOG.md`.

## Preflight

- Confirm release metadata and version wiring:
  - `cargo test --test release -- --nocapture`
- Confirm changelog entry exists for the target version:
  - `CHANGELOG.md` contains `## vX.Y.Z`
- Verify workflow action pinning:
  - `./scripts/verify_action.sh`

## Verification

- Run full release gate:
  - `./scripts/release_gate.sh`
- Run full workspace checks:
  - `cargo fmt --all -- --check`
  - `cargo check --locked`
  - `cargo test --all -- --nocapture`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo build --locked --all-targets`
- Run API contract/integration gate:
  - `cargo test -p codex-api --tests -- --nocapture`

## Live mock demo (release gate)

The live smoke is optional in normal CI and opt-in locally:

```bash
RUN_LIVE_MOCK_SMOKE=true ./scripts/release_gate.sh
```

Success criteria:

- `result: success`
- mixed mode is exercised (`RUN_MOCK_DEMO_MODE=mixed`)
- entrypoint used is `contrib/demo/run_demo.sh`

## Packaging and publish

- Build release binary:
  - `cargo build --locked --release --bin foreman`
- Bundle `templates/` with the binary in release artifacts.
- GitHub release workflow publishes tagged builds (`vX.Y.Z`) with checksums.

## Before tagging

- All required checks are green.
- `CHANGELOG.md` has the final release notes for the exact tag.
- Release artifacts include runtime templates.

Publish:

```bash
git tag -a vX.Y.Z -m "Release vX.Y.Z"
git push origin vX.Y.Z
```
