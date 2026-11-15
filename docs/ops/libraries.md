# Libraries and permissions

A library is a folder of music. A server can have several (for example "Music" on the local
disk and "Vinyl rips" on a NAS), and each person sees only the libraries they were given.
Admins see all of them.

## Adding a library

In the dashboard, **Libraries**, then **Add Library**. Choose a name and the folder as the
server sees it. In Docker that is the path inside the container, so mount each folder first:

```yaml
services:
  trackside:
    volumes:
      - trackside-data:/data
      - /srv/music:/music:ro
      - /mnt/nas/vinyl:/vinyl:ro
```

then add `/vinyl` as a library.

## Scanning

- The first scan reads every file. Expect a few hundred files a second from a local SSD and
  about 80 a second from a NAS over gigabit.
- After that, the server watches for changes and picks up new or edited files within about
  ten seconds. It also runs a quick scan every 30 minutes, which skips folders that haven't
  changed, because network shares often send no change events.
- **Full scan** on a library rereads every tag, for when you've retagged files with a tool
  that keeps modification times.
- A file modified in the last 10 seconds (`scanner.settle`) is probably still being copied, so
  the scan leaves it and imports it in a follow-up pass 15 seconds later. Half-written files
  never appear, and the scan's message says how many files were left for later.
- Songs keep their ids when you retag, rename or move files, even between libraries, so
  playlists, favourites and downloads in Tracks keep working.

To skip a folder, put an empty file called `.trackside-ignore` (or `.nomedia`) in it.

## When a NAS goes away

If a library's folder disappears (the NAS is off, the share isn't mounted, the mount point is
empty), the library shows as unavailable. Nothing is marked missing and nothing is deleted;
songs from it return an error until it's back, and a quick scan runs when it reappears.

If a scan finds less than half the files it knew about (`scanner.empty_guard`), it stops and
says so instead of removing the rest. When the folder really is that much smaller now, choose
**Scan Anyway** on the library's page.

Songs whose files are gone for good are kept as missing for 30 days
(`scanner.missing_grace`), so a file that comes back keeps its id, plays and playlist entries.

## Permissions

Trackside never writes to a library unless you mark it writable (only the uploads and
acquisition libraries need that). It needs to read every file and list every folder.

| Install | Runs as | What to do |
|---|---|---|
| Docker image | uid 65532 | files readable by everyone (`chmod -R a+rX /srv/music`), or run the container as a user that can read them (`user: "1000:1000"`) |
| systemd | a dynamic uid | files readable by everyone; add the folder to `ReadOnlyPaths` |
| Unraid | 99:100 | the usual share permissions work |

If a folder can't be read, the scan records it under the library's problems and leaves its
songs alone instead of marking them missing.

For SMB or NFS mounts on the host, mount them so the server's user can read them, for example
with `uid=`, `gid=` and `file_mode=0644,dir_mode=0755` for CIFS, and use a hard NFS mount
only if you're happy for scans of that library to wait while the NAS is down.

## Uploads and the inbox

With `uploads.enabled`, people allowed to upload can add files from the dashboard or the API.
`uploads.inbox_dir` names a folder that's watched: files dropped into it are tagged, filed into
the uploads library with `uploads.path_template`, and removed from the inbox.
