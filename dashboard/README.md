# Trackside dashboard

The admin pages for Trackside: setup, users and invites, devices and keys, libraries and scans,
backups, settings, activity, your own account, and (when the server has it) song requests.

Plain HTML, CSS and JavaScript modules. No build step, no dependencies, no external URLs. The
server embeds this folder and serves it from `/` (ADR 0007).

## Who opens this, and why

Visits are rare and goal-driven. The overview answers "is everything OK, and if not, what do I
do" before anything else; every other job is one or two clicks from the sidebar.

| Job | Where | How the screen serves it |
|---|---|---|
| First run, often on a laptop with the log open | `/setup`, then Overview | Token, name, password, done. The overview then shows Get Started: add music, add your phone, invite someone, each one button. |
| Invite family | Invites, New Invite | Note, how long it works, access. The link and QR are shown once with a Copy button. |
| Pair a new phone, often on the couch on a phone | My Account, Add a Device | One button shows a QR and a typed code with a countdown; the page notices the new device by itself. A 6-digit code from the app can be approved in the same box. |
| "Music stopped working, what's wrong?" | Overview | Needs Attention lists what is broken with the fix and a button. Playback, Last 24 Hours shows failed plays and when they started next to the History that explains them (a NAS went offline at 08:00). |
| Approve or check requests | Requests | 30-day chart of added and failed, then the list filtered to In Progress, with Details, Try Again and Cancel. |
| Free disk space | Overview Storage, Settings | Free space trend with an honest projection, database and cache sizes; cache limits in Settings. |
| Check a backup exists before an upgrade | Overview Backups, Backups | Last and next backup on the overview; Back Up Now and Download (asks for the password again) on Backups. |
| A family member locked out | Users, a person | Make Reset Link, shown once. |
| A phone was lost | Devices and Keys | Remove next to every device, with who and when it was last used. |

## Files

| Path | What it is |
|---|---|
| `index.html`, `app.js` | Sign-in, the sidebar shell and the hash router (`#/users`, `#/libraries/<id>`, ...) |
| `setup.html`, `setup.js` | First admin account, served at `/setup` |
| `invite.html`, `invite.js` | Accepting an invite, served at `/invite/<token>` |
| `reset.html`, `reset.js` | Choosing a new password, served at `/reset/<token>` |
| `solo.js` | The frame and form pieces those three pages share |
| `api.js` | The only module that calls the server: CSRF header, error codes to messages, route strings |
| `ui.js` | DOM helpers (text only, control and bidi characters stripped), lists, forms, dialogs, toasts, formatting |
| `charts.js` | Hand-written SVG bar and line charts with keyboard access, tooltips and a table view |
| `pages/*.js` | One module per screen; `pages/stats.js` builds the charts from `/admin/stats/*` |
| `style.css` | Every style, light and dark through `prefers-color-scheme`, motion tokens, chart roles |
| `icons/*.svg`, `favicon.svg` | Icons, used as CSS masks so they take the text colour |
| `dev/` | Preview fixtures and the CI check. Not needed at runtime |

`invite.html` and `reset.html` link their assets with `../` because they are served one level
down. `api.js` finds the API at `api/v1/` next to itself, so `server.base_path` works without
any configuration.

## For the server (WP6)

- Serve `/setup` as `setup.html`, `/invite/<token>` as `invite.html`, `/reset/<token>` as
  `reset.html`, and everything else in this folder by path. `dev/` and `*.md` do not need to be
  embedded.
- Headers: `Content-Security-Policy: default-src 'self'; base-uri 'none'; form-action 'self';
  frame-ancestors 'none'; object-src 'none'`, `X-Frame-Options: DENY`,
  `Referrer-Policy: no-referrer`, `X-Content-Type-Options: nosniff`.
  Nothing here needs `'unsafe-inline'`: there are no inline scripts, style attributes or
  `<style>` elements. Widths set from script go through CSSOM (`style.setProperty`), which CSP
  allows.
- The QR images load from `qr_svg_url` (same origin, `image/svg+xml`).
- Requests show in the sidebar only when `GET /api/v1/admin/overview` has an `acquisition` key.
  Leave the key out when the server cannot do acquisition at all; send it with
  `"enabled": false` when it can but is switched off, so an admin can switch it on.

### Shapes the dashboard assumes where api.md is not specific

The fixtures in `dev/fixtures/` are the proposal. In short:

- `GET /admin/devices`: `{"devices": [{id, name, platform, model, app_name, app_version, user: {id, username, display_name}, created_at, last_seen_at, last_ip}]}`
- `GET /admin/api-keys`: `{"keys": [{id, kind, name, prefix, scopes, user: {...}, created_at, last_used_at, last_used_ip}]}`
- `GET /admin/sessions`: `{"sessions": [{id, user: {...}, user_agent, ip, created_at, last_seen_at, current}]}`
- `GET /admin/users/{id}`: AdminUser plus integer `devices`, `api_keys`, `sessions`.
- `GET /admin/settings`: an object keyed by setting name, each `{value, source, editable}`.
- `GET /admin/server/urls`: `{"urls": [{url, kind, tls, source: "detected" | "configured", reachable, checked_at}]}`
- `GET /admin/libraries/{id}/problems`: `{tag_problems: [{song_id, path, problem}], quarantined: [{path, error, at}], unreadable_folders: [{path, since}]}`
- `GET /admin/duplicates`: `{"groups": [{"songs": [{id, title, artist, library_id, path}]}], next_cursor?}`
- `GET /admin/imports`: `{"imports": [{id, file_name, source: "upload" | "inbox", user?, state, song_id?, error?, created_at}]}`
- ScanRun `state`: `running`, `done`, `failed`, `aborted`; `message` explains an aborted run.
- `POST /admin/acquire/providers/{name}/test`: `{ok, elapsed_ms, candidates, detail}`.
- `POST /pair/requests/approve` and `/admin/pairing/approve`: `{"device": {name, platform}}`.
- `GET /admin/acquisitions`: Acquisition plus `user`, `provider`, `candidate: {provider, source, quality, format, size}`, `attempts: [{provider, source, at, error}]`.
- Audit entries: `actor` is a username or `system`; `target` is a display string. The overview
  writes them as sentences (`pages/history.js`) and never shows the target or detail of an
  `acquire.*` action outside the Requests page, since those can name a source.
- Error `details.reason` values with their own message: `set_by_config`, `last_admin`,
  `username_taken`, `running`.

## Preview mode

Open any page with `?preview` and it answers from `dev/fixtures/` instead of the server, with
the clock fixed at 2026-11-14 10:00 UTC so dates read the same on any day.

```sh
cd dashboard && python3 -m http.server 8000
# http://localhost:8000/?preview
```

Variants: `?preview=member` (a member's view), `noacq` (no acquisition: Requests disappears),
`nostats` (an older server without `/admin/stats/*`: the charts are simply absent),
`reauth` (sensitive changes ask for the password again; any password but `wrong` works),
`ratelimit` (sign-in answers 429 with Retry-After),
`fresh` (new server: empty lists and the Get Started steps), `signedout` (sign-in page),
`setup` (open `setup.html?preview=setup`), `slow` (1.5 s per call, to see loading states),
`error` (the overview fails). Invite and reset: `invite.html?preview`, `reset.html?preview`.

`node dev/make-fixtures.mjs` regenerates the fixtures from one script so the numbers agree
across files.

## Checks

`node dashboard/dev/check-coverage.mjs` (Node at CI time only) fails when:

1. an admin endpoint in api.md section 16 is not called by any page,
2. a page calls a GET route that has no preview fixture,
3. any `.html`, `.js` or `.css` file contains `http`,
4. a page has an inline script, style or event handler, or a module uses `innerHTML` and the like,
5. yt-dlp, Soulseek or slskd appear anywhere but `pages/acquisition.js`,
6. a module uses browser storage, cookies, `window.open`, `javascript:` URLs, or the network
   outside `api.js`; `api.js` loses the CSRF header, same-origin credentials or Retry-After; a
   destructive `DELETE` has no confirm dialog beside it,
7. CSS uses `transition: all`, `ease-in`, `scale(0)`, or a `:hover` outside the
   `(hover: hover) and (pointer: fine)` block.

## Charts

| Chart | Where | Why it exists |
|---|---|---|
| Listening, plays per day for 30 days, stacked by person (toggle people in the legend) | Overview | "Is anyone using this, and did it break on a day?" A day with no data is marked and named, never drawn as zero. |
| Playback, last 24 hours: playing at once, converting at once, failed to start, with the conversion limit dashed | Overview | The "music stopped working" answer: when failures started, and whether conversions hit the limit. |
| Free space over 90 days, with a projection only when the trend is steady | Overview, Storage | Freeing space and planning before it runs out. |
| Requests per day, added and failed, 30 days | Requests | Is the request system working. |

Every chart has a figure in words, a legend for two or more series, a table view, and one tab
stop with arrow keys between points. They come from `GET /admin/stats/*` (spec in the WP7
report); a 404 or empty data hides only that chart.

## Motion

One set of tokens in `style.css`: `--ease-out: cubic-bezier(0.23, 1, 0.32, 1)`,
`--ease-in-out`, durations 160 ms (press), 150 ms (colour), 200 ms (enter). Only transform and
opacity move; reduced motion keeps the fades and drops the movement.

## Security

Session cookie only (HttpOnly, set by the server); nothing in browser storage; CSRF header and
same-origin credentials on every call; all text through text nodes with control and bidi
characters removed; secrets shown once and gone when you leave the view; confirm dialogs that
name what happens; a password prompt when the server answers `reauth_required` (and before a
backup download); Retry-After counted down on sign-in; a 401 anywhere returns to sign-in on the
same page; no third-party requests of any kind.

Routes are written as `'METHOD /path/{param}'` strings in the page modules, which is what the
check reads.

## Manual checklist

Do each of these in light and dark mode, and once at 360 px wide. Use a keyboard for at least
one pass.

- [ ] Fresh data directory: open `/setup`, paste the setup token from the log, create the admin.
      The overview shows Get Started.
- [ ] A wrong setup token says so on the token field.
- [ ] Invites: create an invite, open its link in a private window, accept it, land on My Account.
      The invite shows as Used. Open the link again: "This invite has ended".
- [ ] My Account: Show Pairing Code, scan the QR in Tracks. The page notices the new device.
      Cancel a code; let one expire.
- [ ] Device-initiated pairing: start it in Tracks, type the 6 digits under "Got a 6-digit code".
- [ ] Remove that device from My Account and from Devices and Keys. Tracks is signed out.
- [ ] Libraries: add a library, watch the scan count up, open its scan errors.
- [ ] Unmount a library's folder: it shows Offline on the overview and the library page, and
      nothing goes missing. Remount it.
- [ ] Backups: Back Up Now, download it, delete an old one.
- [ ] Settings: change the server name and save; a setting set by config is read-only.
      Leaving with unsaved changes asks first.
- [ ] Users: change a member's libraries; make a reset link and use it; try to delete the last
      admin and read the refusal.
- [ ] Requests: enable a source, run Test, view a failed request's details, Try Again.
- [ ] With acquisition unavailable, Requests is not in the sidebar and `#/requests` goes to the
      overview.
- [ ] Sign in as a member: only My Account is there.
- [ ] Let the session expire (or delete the cookie): the next action shows the sign-in page.

## License

The dashboard is part of Trackside and is licensed like the rest of the repository, under the
[GNU General Public License v3.0 only](../LICENSE) (`GPL-3.0-only`). It has no dependencies
to license separately.
