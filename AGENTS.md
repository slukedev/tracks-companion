## About This Project

- Trackside: a self-hosted music server in Rust, made for the Tracks app (https://github.com/slukedev/tracks) and compatible with Subsonic and OpenSubsonic apps.
- Design: `docs/architecture.md`, `docs/api.md` and `docs/adr/`. Self-hoster docs: `README.md` and `docs/ops/`.
- `tracks-model` comes from the Tracks repository as a git dependency pinned to an exact revision in `Cargo.toml`. Song keys must match the app byte for byte, so move the revision only with app releases.

## Build and Test

- `cargo build`, `cargo test --workspace --locked`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo fmt --all --check`, `cargo deny check`.
- ffmpeg tests: `TRACKSIDE_TEST_FFMPEG=1 cargo test -p trackside-media -p trackside-acquire -- --ignored`.
- End-to-end suite in `tests-e2e/`: `cargo test -p trackside-e2e` (skips without ffmpeg on `PATH`); the 50k-song perf run is `cargo run --release -p trackside-e2e -- --songs 50000 --baseline tests-e2e/baseline-50k.json --report target/e2e-50k.json`.
- Dashboard (`dashboard/`, no build step): `node dashboard/dev/check-coverage.mjs` after any change there or to api.md section 16.
- Packaging: `python3 packaging/scripts/check-packaging.py --allow-missing-metadata`.

## Wire Fixtures

- The fixtures in `crates/trackside-api/tests/fixtures/` are an API contract. The Tracks app keeps a copy in `TracksCore/Tests/TracksCoreTests/Trackside/Fixtures/` and decodes every one in its tests. The app copy leaves out `admin/`, the dashboard's shapes, because they name acquisition providers and the app never does. A fixture change is an API change: re-copy the top-level fixtures into the app repository when they change.
