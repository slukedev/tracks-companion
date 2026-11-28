# Trackside dashboard

The admin pages for Trackside: setup, people (with their devices, app keys and web sessions) and
invites, libraries and scans, speakers through Music Assistant, backups, settings, activity, your
own account, and (when the server has it) song requests.

Plain HTML, CSS and JavaScript modules. No build step, no dependencies, no external URLs. The
server embeds this folder and serves it from `/` (ADR 0007).

## Who opens this, and why

Visits are rare and goal-driven. The overview answers "is everything OK, and if not, what do I
do" before anything else; every other job is one or two clicks from the sidebar.

| Job | Where | How the screen serves it |
|---|---|---|
| First run, often on a laptop with the log open | `/setup`, then Overview | Token, name, password, done. The overview then shows Get started with progress: add music, connect your phone, invite someone, each one button. |
| Invite family | Invites, Create an invite | Who it is for, how long it works, access. The whole link and a QR are shown once, with Share (the device's share sheet, where there is one) and Copy link. The invite page says who invited them and what happens next. |
| Set up a TV, standing in front of it with a phone | The TV's link, or My account, Connect a TV | The code from the TV is filled in already when the phone followed the TV's link. The page shows which TV it is and whether it asked from home, with a countdown, then Approve or Deny; the TV signs itself in. |
| Connect a new phone, often on the couch on a phone | My account, Connect a device | One button shows a QR and a typed code with a live countdown; on a phone, Open in Tracks on this device opens the pairing link directly. The page notices the new device by itself. A 6-digit code from the app can be approved in the same box. |
| "Music stopped working, what's wrong?" | Overview | Needs attention names each problem in plain words with its fix inline: a button named after the action, or the command or setting to copy. Playback shows failed plays and when they started next to Recent activity, which explains them (a NAS went offline at 08:00). |
| Approve or check requests | Requests | The list filtered to In progress, with Details, Try again and Cancel, and a chart of added, cancelled and failed. Source and limits is its own tab. |
| Free disk space | Overview Storage, Settings | Free space of the volume's size, a trend with an honest projection and the month it fills, database and cache sizes; cache limits in Settings. |
| Check a backup exists before an upgrade | Overview Server, Backups | Last and next backup on the overview; Back up now and Download (asks for the password again) on Backups. |
| A family member locked out | People, a person | Make a reset link, shown once, with Share and Copy link. |
| A phone was lost | People, its owner | Remove next to each of their devices, with when and where it was last used. |

## Files

| Path | What it is |
|---|---|
| `index.html`, `app.js` | Sign-in, the shell (sidebar on wide screens; a tab bar and a More sheet on phones) and the hash router (`#/people`, `#/libraries/<id>`, ...). `#/users` and `#/devices` redirect to `#/people` |
| `setup.html`, `setup.js` | First admin account, served at `/setup` |
| `invite.html`, `invite.js` | Accepting an invite, served at `/invite/<token>` |
| `reset.html`, `reset.js` | Choosing a new password, served at `/reset/<token>` |
| `solo.js` | The frame and form pieces those three pages share |
| `api.js` | The only module that calls the server: CSRF header, error codes to messages, route strings |
| `ui.js` | DOM helpers (text only, control and bidi characters stripped), lists, forms, dialogs, toasts, formatting |
| `charts.js` | Hand-written SVG bar and line charts with keyboard access, tooltips and a table view |
| `pages/*.js` | One module per screen; `pages/people.js` is People and a person's page (access, devices, app keys, sessions); `pages/stats.js` builds the charts from `/admin/stats/*` |
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
- `GET /admin/overview`: `acquisition.fetched` is `{songs, keep, bytes, max_bytes, fetching}` for songs
  played without keeping them; the overview's meter follows the fuller of the two limits.
- `PATCH /admin/settings` with `acquire.fetched_keep` or `acquire.fetched_max_bytes` answers `400`
  with a message for the field, which is shown under it.
- Error `details.reason` values with their own message: `set_by_config`, `last_admin`,
  `username_taken`, `running`.

## Preview mode

Open any page with `?preview` and it answers from `dev/fixtures/` instead of the server, with
the clock fixed at 2026-11-14 10:00 UTC so dates read the same on any day.

```sh
cd dashboard && python3 -m http.server 8000
# http://localhost:8000/?preview
```

Variants: `?preview=member` (Mum's view: her own devices, no admin pages), `noacq` (no acquisition: Requests disappears),
`nostats` (an older server without `/admin/stats/*`: the charts are simply absent),
`reauth` (sensitive changes ask for the password again; any password but `wrong` works),
`ratelimit` (sign-in answers 429 with Retry-After),
`fresh` (new server: empty lists and the Get started steps), `signedout` (sign-in page),
`setup` (open `setup.html?preview=setup`), `slow` (1.5 s per call, to see loading states),
`error` (the overview fails).
Speakers: `ma-connecting`, `ma-auth-failed`, `ma-incompatible`, `ma-unreachable`, `ma-setup-required`,
`ma-disabled`, `ma-config` (address and token from the config file, a private CA, a loopback
address and a Music Assistant newer than tested), and for Trackside's registration in it
`needsadmin`, `reg-registering`, `reg-failed`, `reg-stale`, `reg-not-registered`; `fresh` shows
the setup form. Connecting and registering finish by themselves after a moment, a test or token
`wrong` is refused, and a test of an address containing `nowhere` cannot connect. Invite and reset: `invite.html?preview`, `reset.html?preview`;
`invite.html` and `reset.html` link their assets with `../`, so a static server needs
`/dashboard/invite/<token>` and `/dashboard/reset/<token>` rewritten to those files.

Stats fixtures hold the longest range and the preview returns the tail for a shorter one, so the
range controls work. Setting `acquire.fetched_keep` below 0 or `acquire.fetched_max_bytes` to
something that is not a size answers 400, to see the field errors.

`node dev/make-fixtures.mjs` regenerates the fixtures from one script so the numbers agree
across files.

## Checks

`node dashboard/dev/check-coverage.mjs` (Node at CI time only) fails when:

1. an admin endpoint in api.md section 16 is not called by any page,
2. a page calls a GET route that has no preview fixture,
3. any `.html`, `.js` or `.css` file contains `http` (apart from two plain links, the example
   Music Assistant address and the pattern that checks an address has a scheme),
4. a page has an inline script, style or event handler, or a module uses `innerHTML` and the like,
5. yt-dlp appears anywhere but `pages/acquisition.js`,
6. a module uses browser storage, cookies, `window.open`, `javascript:` URLs, or the network
   outside `api.js`; `api.js` loses the CSRF header, same-origin credentials or Retry-After; a
   destructive `DELETE` has no confirm dialog beside it,
7. CSS uses `transition: all`, `ease-in`, `scale(0)`, or a `:hover` outside the
   `(hover: hover) and (pointer: fine)` block.

## Pages and routes

| Route | Page |
|---|---|
| `#/overview` | Status, problems with their fixes, Get started, charts, recent activity, libraries, storage, server facts |
| `#/people`, `#/people/<id>`, `#/people/new` | Everyone with an account; one person's devices, app keys, web sessions, role and access, and sign-in; add a person. A 6-digit code from Tracks is approved at the bottom of the list |
| `#/invites`, `#/invites/new` | Invites waiting and earlier ones; create one |
| `#/libraries`, `#/libraries/<id>`, `#/libraries/add` | Folders, where new songs go, recent uploads (filter and paging), duplicates; one library's scans, problems and settings |
| `#/requests`, `#/requests/<filter>`, `#/requests/setup` | Requests in progress, failed, finished or all; Source and limits on its own tab, with the limits for songs played without keeping them |
| `#/speakers`, `#/speakers/token`, `#/speakers/connection`, `#/speakers/registration` | Music Assistant: its state with one next step, its speakers and streaming services (each linking into Music Assistant, services switchable out of Tracks), Trackside's own source in it, and the connection (address, write-only token, private CA, the address it uses for Trackside, Test connection). The suffixes focus the token or address, or scroll to the registration, for the overview's fixes. `#/music-assistant` lands here. Its service account shows under People with libraries only |
| `#/backups`, `#/settings`, `#/activity`, `#/account` | Backups; settings, with the addresses for Tracks first; the audit log; your own account |
| `#/link`, `#/link/<code>` | Connect a TV: type the TV's 8-character code (or arrive with it from the TV's `/link?code=` address, which the server serves as this page), check the TV's name, kind and whether it asked from your network, then Approve or Deny. Reached from Connect a TV beside Your devices on My account. Expired, used, locked and wrong codes each say what to do next |

A member sees only My account, with no tab bar or menu. On phones (860 px and narrower) an admin
gets a tab bar with Overview, People, Libraries, Requests (Backups when there are no requests) and
More, which raises the full navigation as a sheet.

The dashboard never shows anyone's synced data (history, settings, playlists, likes). The
Listening chart shows plays per person per day as counts only.

## Charts

| Chart | Where | Range | Why it exists |
|---|---|---|---|
| Listening, plays per day stacked by person (toggle people in the legend) | Overview | 7, 30, 90 days | "Is anyone using this, and did it break on a day?" A day with no data is marked and named, never drawn as zero. |
| Playback: playing at once, converting at once, failed to start, with the conversion limit dashed | Overview | 6, 24, 48 hours | The "music stopped working" answer: when failures started, and whether conversions hit the limit. |
| Free space, with a projection only when the trend is steady | Overview, Storage | 30, 90, 365 days | Freeing space and planning before it runs out. The headline is the overview's own free space, so the page shows one number. |
| Requests per day: added, cancelled, failed | Requests | 7, 30, 90 days | Is the request system working. |

Every chart has a figure in words, a legend for two or more series, a table view, and one tab
stop with arrow keys between points. They come from `GET /admin/stats/*` (spec in the WP7
report); a 404 or empty data hides only that chart.

Colours are the dataviz reference steps, re-checked with its validator on `--surface` in both
schemes: people take series 1 to 4 in order (blue, orange, aqua, yellow) and the rest fold into
Other; Playback and Requests use blue, yellow and the reserved status red, which pass the
all-pairs checks in both schemes. Axis and figure numbers are the UI sans with tabular figures;
monospace is kept for paths, commands and codes.

## Motion

One set of tokens in `style.css`: `--ease-out: cubic-bezier(0.23, 1, 0.32, 1)`,
`--ease-in-out`, `--ease-drawer`, durations 160 ms (press), 150 ms (colour), 200 ms (enter),
140 ms (exit). Pages fade up on a route change, dialogs and the save bar enter with
`@starting-style` and leave faster than they came, the phone sheet rises with the drawer curve,
and the pairing countdown bar drains through the Web Animations API, so everything runs on the
compositor at the display's own rate. Nothing is stepped by a timer. Only transform and opacity
move; reduced motion keeps the fades and drops the movement.

## Phones

With a coarse pointer every control is at least 44 px (switches, checkboxes, segmented controls
and links in rows included) and text fields are 16 px, so iOS does not zoom on focus.
`theme-color` follows the colour at the top of the screen: the graphite bar on the dashboard, the
page background on sign-in, setup, invite and reset.

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
- [ ] Invites: create an invite, open its link in a private window, accept it, land on My account.
      The invite shows as Used. Open the link again: "This invite has ended".
- [ ] My account: Show pairing code, scan the QR in Tracks. The page notices the new device.
      On a phone, Open in Tracks on this device opens the app.
      Cancel a code; let one expire.
- [ ] Device-initiated pairing: start it in Tracks, type the 6 digits under "Got a 6-digit code".
- [ ] Connect a TV: start linking on the TV, open its link on a phone (signed out first: sign in,
      then the page opens with the code), Approve; the TV signs in and shows under Your devices.
      Deny another; type a wrong code; let one expire. In preview, codes starting 0, E, D, K, R
      and N show the unknown, expired, used, locked, rate-limited and away-from-home states.
- [ ] Remove that device from My account and from its owner's page under People. Tracks is signed out.
- [ ] Libraries: add a library, watch the scan count up, open its scan errors.
- [ ] Unmount a library's folder: it shows Offline on the overview and the library page, and
      nothing goes missing. Remount it.
- [ ] Backups: Back up now, download it, delete an old one.
- [ ] Settings: change the server name and save; a setting set by config is read-only.
      Leaving with unsaved changes asks first.
- [ ] Settings, Apple Music: choose an `AuthKey_<id>.p8` file (the key ID fills itself in), add
      the team ID and save. Only Saved, the two ids and the date show; Replace and Remove work.
      A pasted text that is not a key says so under the field.
- [ ] People: change a member's libraries; make a reset link and use it; try to delete the last
      admin and read the refusal. Old `#/users` and `#/devices` links land on People.
- [ ] Speakers: on a fresh server, Test connection with a wrong token, then connect with a good
      one; the page shows Connecting and then the speakers and services. Switch a service off and
      on. Register again, Remove (confirm), Add library. Replace the token (asks for the password
      again). Disconnect. A Music Assistant address and token in the config file show locked.
- [ ] Requests: on Source and limits enable the source and run Test search; on Requests view a
      failed request's details and Try again.
- [ ] With acquisition unavailable, Requests is not in the sidebar and `#/requests` goes to the
      overview.
- [ ] Sign in as a member: only My account is there, with no tab bar or menu on a phone.
- [ ] Let the session expire (or delete the cookie): the next action shows the sign-in page.

## License

The dashboard is part of Trackside and is licensed like the rest of the repository, under the
[GNU General Public License v3.0 only](../LICENSE) (`GPL-3.0-only`). It has no dependencies
to license separately.
