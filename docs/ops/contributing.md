# Contributing to the server

The Tracks app's [CONTRIBUTING.md](https://github.com/slukedev/tracks/blob/main/CONTRIBUTING.md)
covers how to propose changes, commit messages, pull requests and conduct, and applies here too.
This page is what's different for the server.

## Setting up

You need [rustup](https://rustup.rs). `rust-toolchain.toml` pins the compiler, and the
first `cargo` command installs it. ffmpeg on your `PATH` is needed only for the tests that use
it. Node is needed only to run the dashboard check.

```sh
cargo build
cargo run -- serve      # data in ~/.local/share/trackside, or set TRACKSIDE_DATA_DIR
```

The dashboard in `dashboard/` is plain HTML, CSS and JavaScript with no build step; its
[README](../../dashboard/README.md) shows how to preview it without a server.

## Before a pull request

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny check                                   # cargo install cargo-deny
TRACKSIDE_TEST_FFMPEG=1 cargo test -p trackside-media -p trackside-acquire
cargo test -p trackside-e2e                        # end-to-end scenarios; they skip without ffmpeg on PATH
node dashboard/dev/check-coverage.mjs              # if you touched the dashboard
```

CI runs the same on Linux, plus the end-to-end suite at 50k songs, which fails on a
regression of more than 25 % against the stored baseline. To run that locally:

```sh
cargo run --release -p trackside-e2e -- --songs 50000 \
  --baseline tests-e2e/baseline-50k.json --max-regression 25 --report target/e2e-50k.json
```

The suite lives in `tests-e2e/`; `tests-e2e/RELEASE.md` has the longer runs done before a
release.

## How the code is held together

- The design is written down first: [architecture.md](../architecture.md),
  [api.md](../api.md) and the decisions in [adr/](../adr/). A change that goes against them
  starts by changing them, in its own pull request, with the reasoning.
- Workspace lints apply everywhere: clippy pedantic, `unwrap_used` denied, `unsafe` forbidden.
- Errors are `thiserror` enums in libraries and `anyhow` only in the binary.
- No blocking work on async tasks. SQLite goes through `Db::read` and `Db::write`, files
  through the blocking pool or the scanner's threads.
- Subprocesses (ffmpeg, yt-dlp) take argument vectors, never a shell, with timeouts and
  `kill_on_drop`.
- Logs never contain secrets. Use the auth crate's redaction helper for any URL.
- Tests use real SQLite and real HTTP on ephemeral ports in preference to mocks, and
  `tokio::time::pause` instead of sleeping. Test names say what happens and when:
  `revoked_key_is_rejected_within_one_second`.
- Comments say why, as in the rest of the repository.

## Things that last

Some things are hard to change once people run the server. Treat changes to them as a design
change with a note in the changelog:

- song, album and artist ids, and how they're derived ([architecture.md section 6](../architecture.md#6-id-strategy));
- the database schema: migrations are forward-only and never rewrite public ids;
- `/api/v1`: additive only;
- config key names: a rename keeps the old name as an alias for a major version;
- the folding in `tracks-model`, pinned from the Tracks repository: a change there needs a server
  migration.

## Packaging changes

Everything in `packaging/` is pinned; [releasing.md](releasing.md#updating-pinned-versions)
says how to move a pin. To try an image change locally (from the repository root):

```sh
docker buildx build -f packaging/docker/Dockerfile --target standard -t trackside:dev --load .
packaging/docker/smoke-test.sh trackside:dev standard
python3 packaging/scripts/check-packaging.py --allow-missing-metadata
docker compose -f packaging/compose.yaml config -q
```

The smoke test starts the image with no configuration on a read-only filesystem and checks the
setup token, the healthcheck and the tools that variant ships.

## Docs

Self-hosters read `README.md` and `docs/ops/`. Keep them in step with what the
server does: a new config key goes in [configuration.md](configuration.md), a new failure
someone can hit goes in [troubleshooting.md](troubleshooting.md). Write plainly, for someone
who runs a home server but doesn't read Rust. Provider names (yt-dlp) appear
only in the server's acquisition docs and dashboard page, never in Tracks or its README.
