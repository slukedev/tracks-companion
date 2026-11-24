# Release checklist

Run from the repository root before tagging `trackside-vX.Y.Z`. `docs/ops/releasing.md` covers
the tag, the changelog and the published artifacts; this list covers what has to be true of
the code first.

## Gate

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo clippy -p trackside-server --no-default-features -- -D warnings`
- [ ] `cargo test --workspace --all-features` with ffmpeg installed and `TRACKSIDE_TEST_FFMPEG=1`,
      so the end-to-end scenarios and the ffmpeg tests run instead of skipping
- [ ] `TRACKSIDE_TEST_FFMPEG=1 cargo test -p trackside-media -p trackside-acquire`
- [ ] `cargo deny check`
- [ ] The end-to-end scenarios also as root on Linux, where the disk-full scenario can mount
      its tmpfs (`sudo -E cargo test -p trackside-e2e --test ops`)

## Performance

- [ ] `cargo run --release -p trackside-e2e -- --songs 50000 --baseline tests-e2e/baseline-50k.json --report target/e2e-50k.json`
      passes (CI runs the same).
- [ ] The 500k run on a medium machine (4-core x86, library on spinning disks, data on SSD)
      and on a Raspberry Pi 5: `--songs 500000 --report target/e2e-500k.json`. Compare every
      metric with its budget in `over_budget` and with the previous release's 500k report.
      A budget miss is a release blocker unless the changelog says why.
- [ ] When a change makes things faster or slower on purpose, replace `baseline-50k.json` with
      the new report from the same CI runner class and say so in the pull request.

## Upgrade and downgrade

- [ ] Start the previous release on a copy of a real data directory, then this one: a
      `backups/pre-migrate-v<from>-v<to>-<time>.db` appears, the server starts, Tracks syncs
      with deltas (no full snapshot unless the release notes say so), song ids are unchanged.
- [ ] Start the previous release again on the migrated directory: it refuses and names the
      pre-migration backup.
- [ ] `trackside backup restore` of that backup brings the previous release back.

## On devices (not testable in CI)

- [ ] Tracks on iPhone pairs by QR code and by typed code, plays over Wi-Fi and cellular, and
      switches between LAN and remote addresses when leaving home.
- [ ] Gapless: an album with continuous audio plays without gaps from converted files on the
      second play (`docs/ops/troubleshooting.md`, "Gaps between songs").
- [ ] AVPlayer seeks inside a direct FLAC and a cached AAC transcode; HLS on cellular starts
      within about a second.
- [ ] Background downloads resume after the app is killed mid-download.
- [ ] A Subsonic app with an app password and one with an API key play, scrobble and show
      covers.
- [ ] Dashboard in Safari, Firefox and Chrome: sign-in, re-authentication prompt, charts,
      no CSP errors in the console.

## Packaging

- [ ] The images' smoke test passes for slim, standard and acquire.
- [ ] `trackside version` in each artifact prints the tag's version.
