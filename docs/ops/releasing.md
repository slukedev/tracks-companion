# Releasing Trackside

For maintainers. A release is a tag named `trackside-vX.Y.Z` on `main`
(`trackside-vX.Y.Z-rc.N` for a pre-release). Pushing it runs
[`.github/workflows/server.yml`](../../../.github/workflows/server.yml), which tests everything,
builds the binaries, packages and images, signs and pushes the images, and creates the GitHub
release from the changelog. The performance and scenario checks that need a person are in
`Server/tests-e2e/RELEASE.md`.

## Checklist

**Release:** Trackside X.Y.Z | **Date:** | **Released by:**

### Before tagging

- [ ] CI is green on `main` for the commit being released, including the end-to-end job at 50k
      songs with no regression over 25 %.
- [ ] The 500k-song end-to-end run from `tests-e2e/RELEASE.md` is within the budgets in
      [architecture.md section 15](../architecture.md#15-performance-budgets).
- [ ] If the database schema changed: the upgrade test from the previous release's database
      passes, a pre-migration backup is written, and the downgrade refusal names it.
- [ ] If `tracks-model` folding changed: the release has the migration that recomputes keys,
      and the golden test passes.
- [ ] No change removes or renames anything in `/api/v1` or a config key without an alias
      ([architecture.md section 13](../architecture.md#versioning)).
- [ ] Pinned tools are current enough: yt-dlp in the `acquire` image is under 30 days old (see
      [Updating pinned versions](#updating-pinned-versions)), ffmpeg and the base images have no
      open security advisories.
- [ ] `version` in `Server/Cargo.toml` is X.Y.Z and `Cargo.lock` is updated.
- [ ] `Server/CHANGELOG.md` has a `## X.Y.Z` section saying what changed for people running
      the server, any action they must take, and whether the database changes.
- [ ] Tried on a real install: the `latest` image upgraded in place over the previous release,
      Tracks connected, played, downloaded and synced.

### Tag

```sh
git switch main && git pull
git tag -s trackside-vX.Y.Z -m "Trackside X.Y.Z"
git push origin trackside-vX.Y.Z
```

### After the workflow finishes

- [ ] The release page has the four binary tarballs, two `.deb`, two `.rpm`, `trackside.rb`
      and `SHA256SUMS`.
- [ ] The images exist for amd64 and arm64:
      `docker buildx imagetools inspect ghcr.io/slukedev/trackside:X.Y.Z`.
- [ ] The signature verifies (command in [install.md](install.md#images-and-tags)).
- [ ] `docker run --rm ghcr.io/slukedev/trackside:X.Y.Z version` prints X.Y.Z.
- [ ] Copy `trackside.rb` from the release into the tap repository and push it;
      `brew install slukedev/tap/trackside` installs X.Y.Z.
- [ ] Unraid, TrueNAS, Synology and CasaOS templates still point at the right tags.
- [ ] Watch issues and discussions for a day.

### When to pull a release

- The server doesn't start, or doesn't reach ready, after upgrading from the previous release.
- Song ids change across the upgrade.
- Sign-in, playback or sync fails for Tracks.

To pull one: mark the GitHub release as a pre-release, move the `latest`, `slim`, `acquire` and
`X.Y` tags back to the previous release
(`docker buildx imagetools create -t ghcr.io/slukedev/trackside:latest ghcr.io/slukedev/trackside:<previous>`,
and the same for the other names), and say in the release notes what to do. Never delete a
pushed version tag; publish a fixed X.Y.Z+1 instead.

## Updating pinned versions

Everything the images and CI use is pinned by version and digest or checksum. To move one,
change both together.

| What | Where | How to find the new pin |
|---|---|---|
| Base images, ffmpeg, Deno, alpine, debian | `ARG` lines at the top of `packaging/docker/Dockerfile` | `docker buildx imagetools inspect <image>:<tag>` and copy `Digest` |
| yt-dlp | `YTDLP_VERSION` and the two `YTDLP_SHA256_*` lines | the release's `SHA2-256SUMS`, lines for `yt-dlp_linux.zip` and `yt-dlp_linux_aarch64.zip` |
| slskd, Tailscale | `packaging/compose.acquire.yaml`, `packaging/compose.tailscale.yaml` | `docker buildx imagetools inspect` |
| ffmpeg in CI | `FFMPEG_IMAGE` in the workflow | same digest as the Dockerfile |
| Rust | `rust-toolchain.toml` and `RUST_VERSION` in the Dockerfile | both together; CI checks they match |
| GitHub Actions | `uses:` lines in the workflow | `git ls-remote --tags https://github.com/<owner>/<action>`, the commit of the tag |
| CI tools | `tool:` lines (cargo-deny, cargo-zigbuild, cargo-deb, cargo-generate-rpm), `ziglang` | crates.io and PyPI |

After a change, build each image target, run
[`packaging/docker/smoke-test.sh`](../../packaging/docker/smoke-test.sh) on it, and for
yt-dlp, run a search and a download through a test server. A new yt-dlp release can change
command-line options; check the provider's arguments against its changelog.
