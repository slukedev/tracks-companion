# Upgrading and downgrading

## How upgrades work

- Releases follow semantic versioning. Within a major version, nothing you depend on is
  removed: the native API only gains things, config keys that get renamed keep their old name
  as an alias until the next major version, and Subsonic behaviour stays the same.
- When a new version needs a database change, it first writes
  `backups/pre-migrate-v<from>-v<to>-<time>.db`, then applies the change in a transaction. A
  crash or power cut in the middle leaves the old schema in place, and the next start tries
  again.
- Song, album and artist ids never change in an upgrade, so Tracks and other apps keep their
  playlists, favourites and downloads.
- The [changelog](../../CHANGELOG.md) says which releases change the database and anything you
  need to do.

## Checklist

Before:

- [ ] Read the changelog entries between your version and the new one.
- [ ] Check the last nightly backup exists (**Backups** in the dashboard), or make one:
      `trackside backup create`.
- [ ] Copy `backups/` and `secret.key` somewhere off the server if you haven't lately.
- [ ] Check there's free space on the data disk for at least one more copy of the database.

Upgrade:

- [ ] Docker Compose: change `TRACKSIDE_TAG` in `.env` (or keep `latest`), then
      `docker compose pull && docker compose up -d`.
- [ ] Docker Compose with `compose.build.yaml`: change `TRACKSIDE_BUILD_REF` in `.env` if
      you pin one, then `docker compose up -d --build`.
- [ ] Docker: `docker pull ghcr.io/slukedev/trackside:<version>`, then remove and
      recreate the container with the same volumes.
- [ ] `.deb` or `.rpm`: install the new package; the service restarts by itself.
- [ ] Binary: replace the file and restart the service.

After:

- [ ] The log shows `database ready` with the new `schema` number and, if the database
      changed, `pre-migration backup written` just before it. `trackside version` prints the
      new version.
- [ ] `curl -s http://localhost:4747/readyz` says `"status": "ready"`.
- [ ] The dashboard's overview has no new warnings.
- [ ] Tracks connects, plays a song and shows recent changes.

Roll back when:

- the server doesn't reach ready within a few minutes and the log names an error you can't fix;
- playback or sign-in fails for everyone after the upgrade.

## Downgrading

A database that a newer version has migrated can't be opened by an older one. The older server
refuses to start, says the database is from a newer Trackside, and names the backups folder.
To go back, restore the `pre-migrate-v<from>-v<to>-<time>.db` written by that upgrade:

```sh
docker compose stop trackside
# set TRACKSIDE_TAG back to the old version in .env
docker compose run --rm trackside backup restore /data/backups/pre-migrate-v3-v4-20261114T101500Z.db
docker compose up -d
```

Anything that changed after the upgrade (new listens, playlists, accounts) is lost with the
restore. Tracks keeps unsent listens in its own outbox, so recent plays from phones are sent
again.

If the release didn't change the database, just run the old version; no restore is needed.

## Building from main

No images are published between releases. To run `main`, build it on your machine with
[`compose.build.yaml`](install.md#building-from-source-with-compose). Every commit on `main`
has passed the full checks, but nobody has used it yet, and it may break. Run it on a second
server, or with a backup you're ready to restore.

- **Update:** `docker compose up -d --build`. Docker fetches `main` again and rebuilds only
  what changed, which usually takes a few minutes rather than the length of a first build.
- **Stay on one build:** set `TRACKSIDE_BUILD_REF` in `.env` to a full commit hash, and move it
  on purpose.
- **Go back to a release:** set `TRACKSIDE_BUILD_REF` to the release tag
  (`trackside-vX.Y.Z`), or remove `compose.build.yaml` from `COMPOSE_FILE` and pull the
  release image. If `main` had a newer database schema, restore a backup as in
  [Downgrading](#downgrading).
