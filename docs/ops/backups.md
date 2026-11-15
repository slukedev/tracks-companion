# Backups and restore

## What to keep

Everything Trackside knows is in its data folder (`/data` in Docker):

| Path | What | Back up? |
|---|---|---|
| `trackside.db` (with `-wal` and `-shm` while running) | accounts, libraries, song ids, listens, favourites, playlists, settings | yes, through a backup file, never by copying the live file |
| `secret.key` | the key that seals app passwords and signs media tokens | yes, and keep it private |
| `backups/` | the server's own backups | copy it somewhere else |
| `config.toml` | your settings, if you made one | yes |
| `cache/` | artwork and converted songs | no, it rebuilds itself |
| `acquire/staging/` | downloads in progress | no |

Your music isn't in the data folder and Trackside never changes it, so back it up the way you
already do.

If `secret.key` is lost, app passwords and pending pairing codes stop working and have to be
made again. Passwords, sessions and device keys keep working, because they're stored as
hashes.

## Automatic backups

Every night at 03:30 server time the server writes a consistent copy of the database to
`backups/trackside-<UTC time>.db`, with `secret.key` beside it as `<same name>.key`, and keeps
the last 7. It checks there's room first; if there isn't, it skips the backup and says so on the
dashboard's overview.

```toml
[backup]
schedule = "daily 03:30"   # or "weekly sun 04:00", or "off"
keep = 7
dir = "/data/backups"      # can point at another disk
include_secret_key = true
```

Before any upgrade that changes the database, the server also writes
`pre-migrate-v<from>-v<to>-<time>.db`. Those and manual backups are kept until you delete
them.

These backups sit on the same disk as the database. Copy `backups/` somewhere else too: a
NAS, a USB disk, or cloud storage with restic, Borg or rclone.

## Manual backups

From the dashboard (**Backups**), or on the host:

```sh
trackside backup create
trackside backup list
```

In Docker, run them inside the container:

```sh
docker compose exec trackside trackside backup create
```

## Restore

Restoring replaces the database, so the server has to be stopped. The command refuses while it
runs.

```sh
# Docker Compose
docker compose stop trackside
docker compose run --rm trackside backup restore /data/backups/trackside-20261114T033000Z.db
docker compose start trackside

# systemd
sudo systemctl stop trackside
sudo systemd-run --wait --pty -p DynamicUser=yes -p StateDirectory=trackside \
  /usr/bin/trackside backup restore /var/lib/trackside/backups/trackside-20261114T033000Z.db
sudo systemctl start trackside
```

`systemd-run` runs the restore as the service's own dynamic user, so the restored file has
the right owner.

`restore` checks the file's integrity, moves the current database aside as
`trackside.db.before-restore-<time>`, and copies the backup in. If a `.key` file sits next to
the backup, copy it over `secret.key` too.

## Continuous replication with Litestream

For a copy that's never more than a few seconds behind, run
[Litestream](https://litestream.io) next to the server. It reads the database's write-ahead
log and streams it to S3, Backblaze B2, MinIO or another folder. Trackside uses WAL mode and
leaves checkpointing alone, which is what Litestream needs.

`litestream.yml`:

```yaml
dbs:
  - path: /data/trackside.db
    replica:
      type: s3
      bucket: my-trackside-backups
      path: trackside
      endpoint: https://s3.eu-central-003.backblazeb2.com
```

Added to `compose.yaml`:

```yaml
services:
  litestream:
    image: docker.io/litestream/litestream:0.5.18@sha256:513bff62a88fc0c86c73d27333c41c54c89fe890a7c32a551600adff9309b11b
    restart: unless-stopped
    user: "65532:65532"
    command: replicate
    volumes:
      - trackside-data:/data
      - ./litestream.yml:/etc/litestream.yml:ro
    environment:
      LITESTREAM_ACCESS_KEY_ID: ${LITESTREAM_ACCESS_KEY_ID}
      LITESTREAM_SECRET_ACCESS_KEY: ${LITESTREAM_SECRET_ACCESS_KEY}
```

To restore from it, stop Trackside and run
`docker compose run --rm litestream restore -o /data/trackside.db /data/trackside.db`
after moving the old file aside. Litestream doesn't copy `secret.key`; keep that with your
other backups.

## Your listening history

Everyone can download their own listens as JSON Lines in the ListenBrainz format, from
**My Account** in the dashboard or `GET /api/v1/me/listens/export`.
