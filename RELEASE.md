# Release checklist for foreman

Use this checklist before publishing any tag/release.

## Preflight

- Confirm release versioning consistency:
  - `cargo test --test release -- --nocapture`
- Verify changelog/release notes are ready for the target version:
  - `CHANGELOG.md` contains `## vX.Y.Z`
  - `RELEASES.md` contains `## vX.Y.Z`
- Verify workflow hardening:
  - `./scripts/verify_action_shas.sh`

## API + regression checks

- Run normal workspace verification:
  - `cargo fmt --all -- --check`
  - `cargo check --locked`
  - `cargo test --all -- --nocapture`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo build --locked --all-targets`
- Run crate-level gate:
  - `cargo test -p codex-api --tests -- --nocapture`

## Release gate script

Run the release gate script:

```bash
./scripts/release_gate.sh
```

If you want to run the real mixed live smoke locally, opt in explicitly:

```bash
RUN_LIVE_MOCK_SMOKE=true ./scripts/release_gate.sh
```

## Manual doc check

- `README.md` references the live mock demo entrypoints.
- `docs/manual.md` documents mixed-mode run/criteria.
- `TESTING.md` and `RELEASES.md` reference the mixed-mode gate requirements.

## Packaging

- Build release artifacts:
  - `cargo build --locked --release`
- Follow `RELEASES.md` packaging instructions for template paths and install layout.
