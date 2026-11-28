// Writes dev/fixtures/*.json: synthetic data in the shapes of api.md. Run with Node: node dev/make-fixtures.mjs
import { writeFileSync } from 'node:fs';
const out = process.argv[2] ?? new URL('./fixtures', import.meta.url).pathname;
const w = (name, obj) => writeFileSync(`${out}/${name}.json`, JSON.stringify(obj, null, 2) + '\n');

const SERVER_ID = '5f0c2b9e-8a3d-4c1e-9b7a-2d6e4f8a1c3b';
const urls = [
  { url: 'http://192.168.1.20:4747', kind: 'lan', tls: false },
  { url: 'http://home.local:4747', kind: 'lan', tls: false },
  { url: 'https://home.tail4a2c.ts.net', kind: 'tailnet', tls: true },
];

const perms = (o = {}) => ({ can_download: true, can_upload: false, can_request: false, legacy_subsonic: false, can_use_speakers: true, can_play_services: true, ...o });
const LIB = {
  music: { id: 'l0a8f2c9e7d1b3k5m6n4', name: 'Music', state: 'ok', song_count: 48211 },
  nas: { id: 'l7c2d9f4a1e8b6m3n5k0', name: 'NAS Archive', state: 'unavailable', song_count: 12904 },
  acquired: { id: 'l3m9q2w8e5r1t7y4u6i0', name: 'Acquired', state: 'ok', song_count: 312 },
};
const allLibs = [LIB.music.id, LIB.nas.id, LIB.acquired.id];

const users = [
  { id: 'u8b1c4d7e0f3a6b9c2d5', username: 'luke', display_name: 'Luke', role: 'admin', permissions: perms({ can_upload: true, can_request: true, legacy_subsonic: true }), library_ids: allLibs, created_at: '2026-09-02T18:11:04.000Z', last_seen_at: '2026-11-14T09:58:12.000Z', disabled: false },
  { id: 'u2e5f8a1b4c7d0e3f6a9', username: 'mum', display_name: 'Mum', role: 'member', permissions: perms({ can_request: true }), library_ids: [LIB.music.id, LIB.acquired.id], created_at: '2026-09-04T10:02:51.000Z', last_seen_at: '2026-11-14T07:41:30.000Z', disabled: false },
  { id: 'u5a8b1c4d7e0f3a6b9c2', username: 'sam', display_name: 'Sam Okafor', role: 'member', permissions: perms({ legacy_subsonic: true, max_bitrate_kbps: 320 }), library_ids: [LIB.music.id], created_at: '2026-10-19T20:45:00.000Z', last_seen_at: '2026-11-13T21:10:44.000Z', disabled: false },
  { id: 'u9d2e5f8a1b4c7d0e3f6', username: 'grandad', display_name: 'Grandad', role: 'member', permissions: perms({ can_download: false }), library_ids: [LIB.music.id], created_at: '2026-09-20T13:30:00.000Z', last_seen_at: '2026-10-02T16:05:10.000Z', disabled: true },
];
// The account Trackside made for Music Assistant: no password, cannot sign in, keys on /rest only.
const serviceUser = { id: 'uvxvacxf5ak9afxq4qdjj', username: 'music-assistant', display_name: 'Music Assistant', role: 'member', service: 'music_assistant', permissions: perms({ can_download: false }), library_ids: allLibs, created_at: '2026-11-12T18:00:04.000Z', last_seen_at: '2026-11-14T09:29:00.000Z', disabled: false };
users.push(serviceUser);
const ref = (u) => ({ id: u.id, username: u.username, display_name: u.display_name });
const [luke, mum, sam, grandad] = users;

const scanDone = { id: 'n4r8t2y6u0i3o7p1a5s9', library_id: LIB.music.id, kind: 'full', trigger: 'admin', state: 'done', started_at: '2026-11-13T18:05:02.000Z', finished_at: '2026-11-13T18:19:47.000Z', folders_seen: 4112, files_seen: 48214, added: 212, updated: 37, moved: 3, missing: 1, errors: 2 };
const scanNas = { id: 'n1q5w9e3r7t1y5u9i3o7', library_id: LIB.nas.id, kind: 'quick', trigger: 'schedule', state: 'aborted', started_at: '2026-11-14T08:00:00.000Z', finished_at: '2026-11-14T08:00:05.000Z', folders_seen: 0, files_seen: 0, added: 0, updated: 0, moved: 0, missing: 0, errors: 0, message: 'The library root is empty or not mounted. Nothing was marked missing.' };
const scanAcq = { id: 'n8z2x6c0v4b8n2m6q0w4', library_id: LIB.acquired.id, kind: 'quick', trigger: 'import', state: 'done', started_at: '2026-11-14T09:31:12.000Z', finished_at: '2026-11-14T09:31:13.000Z', folders_seen: 41, files_seen: 312, added: 1, updated: 0, moved: 0, missing: 0, errors: 0 };

const providers = [
  { name: 'ytdlp', enabled: true, configured: true, health: 'ok', detail: 'yt-dlp 2026.10.22', version: '2026.10.22', latest_known: '2026.11.03', settings: { concurrency: 2, search_rate: '10/1m', cookies_file: '/data/cookies.txt' }, stats: { searches_today: 31, fetches_today: 12, failures_today: 1 } },
];

const overview = {
  server: { name: 'Home', server_id: SERVER_ID, version: '1.0.0', uptime_s: 266400, started_at: '2026-11-11T08:00:00.000Z', data_dir: '/data', tls: 'off', listen: ['[::]:4747'] },
  storage: { database_bytes: 812000000, wal_bytes: 4100000, cache_bytes: 3200000000, data_free_bytes: 120000000000, warnings: [] },
  libraries: [
    { ...LIB.music, last_scan: scanDone, scanning: false },
    { ...LIB.nas, last_scan: scanNas, scanning: false },
    { ...LIB.acquired, last_scan: scanAcq, scanning: false },
  ],
  media: { ffmpeg: { found: true, version: '7.1' }, transcodes: { running: 1, queued: 0 }, transcode_cache_bytes: 2100000000 },
  acquisition: { enabled: true, providers, active: 2, today: 14, fetched: { bytes: 7400000000, fetching: 1, keep: 500, max_bytes: 21474836480, songs: 212 } },
  users: { count: 4, devices: 7 },
  backups: { last: { name: 'trackside-20261114T033000Z.db', at: '2026-11-14T03:30:00.000Z', bytes: 790000000 }, next_at: '2026-11-15T03:30:00.000Z' },
  problems: [
    { severity: 'warning', code: 'library_unavailable', title: 'NAS Archive offline', message: 'NAS Archive has been unavailable since 08:00.', help: 'Check that the NAS is on and its share is mounted at /mnt/nas/music. Its songs stay in the library until it is back.', target: { kind: 'library', id: LIB.nas.id } },
    { severity: 'warning', code: 'inotify_limit', title: 'Music not watched live', message: 'Watching Music for changes fell back to polling.', help: 'Raise fs.inotify.max_user_watches to 524288 on the host so new files appear within seconds.', target: { kind: 'library', id: LIB.music.id } },
  ],
  music_assistant: { registration: 'registered', relays: 1, services: 2, speakers: 5, state: 'connected' },
};
w('admin-overview', overview);

// GET /readyz (server root): the shape of trackside_api::wire::Ready.
w('readyz', {
  status: 'ready',
  checks: { database: 'ok', ffmpeg: 'ok', libraries: 'degraded', migrations: 'ok' },
  notes: [
    `Library ${LIB.nas.id} is unavailable`,
    'Requests arrive through a proxy that is not in server.trusted_proxies. Add it so each client is limited and logged by its own address, and so passwords and keys are accepted through it.',
  ],
});

const noacq = structuredClone(overview);
delete noacq.acquisition;
noacq.music_assistant = { state: 'not_set_up' };
noacq.libraries = noacq.libraries.filter((l) => l.id !== LIB.acquired.id);
w('admin-overview-noacq', noacq);

const fresh = {
  server: overview.server,
  storage: { database_bytes: 98304, wal_bytes: 0, cache_bytes: 0, data_free_bytes: 120000000000, warnings: [] },
  libraries: [],
  media: { ffmpeg: { found: false }, transcodes: { running: 0, queued: 0 }, transcode_cache_bytes: 0 },
  users: { count: 1, devices: 0 },
  backups: { next_at: '2026-11-15T03:30:00.000Z' },
  problems: [{ severity: 'info', code: 'no_ffmpeg', title: 'ffmpeg not found', message: 'ffmpeg was not found, so songs play only in their original format.', help: 'Use the standard image, or install ffmpeg and restart Trackside.' }],
};
w('admin-overview-fresh', fresh);

const sessionUser = { id: luke.id, username: luke.username, display_name: luke.display_name, role: 'admin', permissions: luke.permissions };
w('auth-session', sessionUser);
w('auth-session-member', { id: mum.id, username: mum.username, display_name: mum.display_name, role: 'member', permissions: mum.permissions });

w('capabilities', {
  product: 'trackside', server_id: SERVER_ID, name: 'Home', version: '1.0.0',
  api: { versions: ['v1'], current: 'v1', deprecated: [] }, setup_required: false,
  auth: { password_login: true, pairing: true, pair_requests: true, device_link: true },
  media: { transcoding: true, profiles: ['aac-256', 'aac-128', 'mp3-320', 'mp3-128', 'opus-128', 'flac'], hls: true, suggested_cellular_profile: 'aac-256' },
  features: { lyrics: true, uploads: false, events: true, animated_art: false, speakers: true },
  subsonic: { enabled: true, path: '/rest' },
});
w('setup-status', { setup_required: false });
w('setup-status-required', { setup_required: true });

w('me', {
  user: sessionUser,
  libraries: [LIB.music, LIB.nas, LIB.acquired],
  server: { server_id: SERVER_ID, name: 'Home', version: '1.0.0', urls },
  features: { acquisition: { instant_play: true }, apple_music: { developer_token: true }, catalog: { relay: true, relay_cellular: true }, speakers: { control: true } },
});

w('admin-users', { users });
w('admin-user', { ...mum, devices: 2, api_keys: 0, sessions: 1, cloud: { items: 12400, bytes: 18000000 } });
w('admin-user-created', { ...users[2], id: 'u0f3a6b9c2d5e8f1a4b7', username: 'nana', display_name: 'Nana', created_at: '2026-11-14T10:00:00.000Z', last_seen_at: undefined });
w('admin-password-reset', { url: 'http://192.168.1.20:4747/reset/preview-reset-token-4XQ9', expires_at: '2026-11-15T10:00:00.000Z' });

const invites = [
  { id: 'v3b6n9m2q5w8e1r4t7y0', role: 'member', permissions: perms({ can_request: true }), library_ids: [LIB.music.id, LIB.acquired.id], note: 'For Nana', created_by: 'luke', created_at: '2026-11-14T09:15:20.000Z', expires_at: '2026-11-21T09:15:20.000Z', state: 'open' },
  { id: 'v7y0t3r6e9w2q5m8n1b4', role: 'member', permissions: perms({ legacy_subsonic: true }), library_ids: [LIB.music.id], note: 'Sam', created_by: 'luke', created_at: '2026-10-19T20:30:00.000Z', expires_at: '2026-10-26T20:30:00.000Z', state: 'used', used_by: 'sam' },
  { id: 'v1a4s7d0f3g6h9j2k5l8', role: 'member', permissions: perms(), library_ids: [LIB.music.id], note: 'Old link for the cousins', created_by: 'luke', created_at: '2026-09-21T11:00:00.000Z', expires_at: '2026-09-28T11:00:00.000Z', state: 'expired' },
];
w('admin-invites', { invites });
w('admin-invite-created', { ...invites[0], id: 'v9k2l5z8x1c4v7b0n3m6', note: '', created_at: '2026-11-14T10:00:00.000Z', expires_at: '2026-11-21T10:00:00.000Z', url: 'http://192.168.1.20:4747/invite/preview-invite-token-7KQ2M9XD', qr_svg_url: '/api/v1/admin/invites/v9k2l5z8x1c4v7b0n3m6/qr.svg' });
w('invite-info', { valid: true, role: 'member', expires_at: '2026-11-21T09:15:20.000Z', invited_by: 'Luke', note: 'For Mum' });

const adminLib = (l, o) => ({ ...l, enabled: true, writable: false, watch: true, quick_scan_interval_s: 1800, watching: 'events', uploads_target: false, acquire_target: false, ...o });
w('admin-libraries', { libraries: [
  adminLib(LIB.music, { root: '/music', watching: 'polling' }),
  adminLib(LIB.nas, { root: '/mnt/nas/music', unavailable_reason: 'The folder is empty or not mounted.', unavailable_since: '2026-11-14T08:00:00.000Z' }),
  adminLib(LIB.acquired, { root: '/acquired', writable: true, acquire_target: true, uploads_target: true }),
] });
w('admin-library-created', adminLib({ id: 'l6p9o2i5u8y1t4r7e0w3', name: 'Audiobooks', state: 'ok', song_count: 0 }, { root: '/srv/audiobooks' }));
w('admin-library-scans', { scans: [
  scanDone,
  { ...scanDone, id: 'n9o3i7u1y5t9r3e7w1q5', kind: 'quick', trigger: 'schedule', started_at: '2026-11-13T17:30:00.000Z', finished_at: '2026-11-13T17:30:09.000Z', folders_seen: 4112, files_seen: 48001, added: 0, updated: 0, moved: 0, missing: 0, errors: 0 },
  { ...scanDone, id: 'n2w6e0r4t8y2u6i0o4p8', kind: 'quick', trigger: 'watch', started_at: '2026-11-12T22:14:41.000Z', finished_at: '2026-11-12T22:14:44.000Z', folders_seen: 3, files_seen: 14, added: 14, updated: 0, moved: 0, missing: 0, errors: 0 },
] });
w('admin-library-scans-nas', { scans: [scanNas] });
w('admin-library-scans-running', { scans: [{ id: 'n5t9y3u7i1o5p9a3s7d1', library_id: LIB.acquired.id, kind: 'full', trigger: 'admin', state: 'running', started_at: '2026-11-14T09:59:40.000Z', folders_seen: 22, files_seen: 168, added: 0, updated: 0, moved: 0, missing: 0, errors: 0 }, scanAcq] });
w('admin-scan', { ...scanDone, errors: [
  { path: 'Various Artists/Now 47/disc 2/07 Track07.mp3', error: 'Tags could not be read; the title was taken from the file name.' },
  { path: 'Björk/Vespertine/.DS_Store', error: 'Not an audio file.' },
] });
w('admin-scan-started', { id: 'n0p4o8i2u6y0t4r8e2w6', library_id: LIB.music.id, kind: 'quick', trigger: 'admin', state: 'running', started_at: '2026-11-14T10:00:00.000Z', folders_seen: 0, files_seen: 0, added: 0, updated: 0, moved: 0, missing: 0, errors: 0 });
w('admin-library-problems', {
  tag_problems: [{ song_id: 's7m2k4q9x1v8b3n6c0de', path: 'Various Artists/Now 47/disc 2/07 Track07.mp3', problem: 'Tags could not be read; the title was taken from the file name.' }],
  quarantined: [{ path: 'Aphex Twin/Drukqs/broken.flac', error: 'Reading this file took longer than 30 seconds.', at: '2026-11-13T18:12:03.000Z' }],
  unreadable_folders: [],
});
w('admin-duplicates', { groups: [
  { songs: [
    { id: 's1d3f5g7h9j1k3l5z7x9', title: 'Halo', artist: 'Beyoncé', library_id: LIB.music.id, path: 'Beyoncé/I Am... Sasha Fierce/02 Halo.flac' },
    { id: 's2f4g6h8j0k2l4z6x8c0', title: 'Halo', artist: 'Beyoncé', library_id: LIB.acquired.id, path: 'Beyoncé/I Am... Sasha Fierce/Halo.flac' },
  ] },
] });
w('admin-imports', { next_cursor: 'aW1wb3J0czoy', imports: [
  { id: 'i5q8w1e4r7t0y3u6i9o2', file_name: '01 Halo.flac', source: 'upload', user: ref(luke), state: 'imported', song_id: 's2f4g6h8j0k2l4z6x8c0', created_at: '2026-11-14T09:30:58.000Z' },
  { id: 'i2w5e8r1t4y7u0i3o6p9', file_name: 'voice memo.m4a', source: 'inbox', state: 'rejected', error: 'The file has a video track.', created_at: '2026-11-13T12:02:11.000Z' },
] });
w('admin-targets', { uploads_library: LIB.acquired.id, acquire_library: LIB.acquired.id });

const devices = [
  { id: 'd2c4e6g8i0k2m4o6q8s0', name: "Luke's iPhone", platform: 'ios', model: 'iPhone18,1', app_name: 'Tracks', app_version: '3.2', user: ref(luke), created_at: '2026-09-02T18:20:00.000Z', last_seen_at: '2026-11-14T09:58:12.000Z', last_ip: '192.168.1.41' },
  { id: 'd4e6g8i0k2m4o6q8s0u2', name: "Luke's MacBook Pro", platform: 'macos', model: 'Mac16,1', app_name: 'Tracks', app_version: '3.2', user: ref(luke), created_at: '2026-09-02T18:31:00.000Z', last_seen_at: '2026-11-13T23:12:40.000Z', last_ip: '100.84.12.7' },
  { id: 'd6g8i0k2m4o6q8s0u2w4', name: 'Living Room Mac', platform: 'macos', model: 'Macmini9,1', app_name: 'Tracks', app_version: '3.2', user: ref(luke), created_at: '2026-11-14T09:42:10.000Z', last_seen_at: '2026-11-14T09:42:10.000Z', last_ip: '192.168.1.60' },
  { id: 'd8i0k2m4o6q8s0u2w4y6', name: "Mum's iPad", platform: 'ipados', model: 'iPad14,1', app_name: 'Tracks', app_version: '3.1', user: ref(mum), created_at: '2026-09-04T10:10:00.000Z', last_seen_at: '2026-11-14T07:41:30.000Z', last_ip: '192.168.1.52' },
  { id: 'd0k2m4o6q8s0u2w4y6a8', name: "Mum's iPhone", platform: 'ios', model: 'iPhone15,4', app_name: 'Tracks', app_version: '3.2', user: ref(mum), created_at: '2026-09-04T10:12:00.000Z', last_seen_at: '2026-11-12T19:03:00.000Z', last_ip: '100.71.3.22' },
  { id: 'd1m3o5q7s9u1w3y5a7c9', name: "Sam's iPhone", platform: 'ios', model: 'iPhone17,2', app_name: 'Tracks', app_version: '3.2', user: ref(sam), created_at: '2026-11-13T21:10:44.000Z', last_seen_at: '2026-11-13T21:10:44.000Z', last_ip: '100.90.41.5' },
  { id: 'd3o5q7s9u1w3y5a7c9e1', name: "Grandad's iPad", platform: 'ipados', model: 'iPad12,1', app_name: 'Tracks', app_version: '3.0', user: ref(grandad), created_at: '2026-09-20T13:40:00.000Z', last_seen_at: '2026-10-02T16:05:10.000Z', last_ip: '192.168.1.77' },
];
w('admin-devices', { devices });
w('admin-api-keys', { keys: [
  { id: 'k7b9d1f3h5j7l9n1p3r5', kind: 'app_password', name: 'Symfonium', prefix: 'TRK7', scopes: ['library', 'stream', 'write'], user: ref(sam), created_at: '2026-10-19T21:00:00.000Z', last_used_at: '2026-11-13T22:40:11.000Z', last_used_ip: '100.90.41.5' },
  { id: 'k2c4e6g8i0k2m4o6q8s0', kind: 'api_key', name: 'Home Assistant', prefix: 'tsk_Hq7', scopes: ['library'], user: ref(luke), created_at: '2026-10-01T09:00:00.000Z', last_used_at: '2026-11-14T09:55:00.000Z', last_used_ip: '192.168.1.10' },
  { id: 'kwd5w7vf6ptaf3cwd1xbt', kind: 'api_key', name: 'Music Assistant', prefix: 'tsk_Qm3vXa', scopes: ['library', 'stream'], managed_by: 'music_assistant', user: ref(serviceUser), created_at: '2026-11-12T18:00:05.000Z', last_used_at: '2026-11-14T09:29:00.000Z', last_used_ip: '192.168.1.20' },
] });
w('admin-sessions', { sessions: [
  { id: 'e1s3s5i7o9n1a3b5c7d9', user: ref(luke), user_agent: 'Safari on macOS', ip: '192.168.1.41', created_at: '2026-11-10T19:00:00.000Z', last_seen_at: '2026-11-14T10:00:00.000Z', current: true },
  { id: 'e2s4s6i8o0n2a4b6c8d0', user: ref(mum), user_agent: 'Safari on iPadOS', ip: '192.168.1.52', created_at: '2026-11-01T08:00:00.000Z', last_seen_at: '2026-11-14T07:40:00.000Z', current: false },
] });
w('admin-pairing-approved', { device: { name: 'Living Room Mac', platform: 'macos' } });

const s = (value, source = 'default', editable = true) => ({ value, source, editable });
w('admin-settings', {
  'server.name': s('Home', 'dashboard'),
  'server.public_urls': s(['https://home.tail4a2c.ts.net'], 'dashboard'),
  'scanner.watch': s(true),
  'scanner.quick_interval': s('30m'),
  'scanner.full_interval': s('off'),
  'scanner.missing_grace': s('30d'),
  'media.transcode_cache_max': s('10GiB', 'config', false),
  'media.art_cache_max': s('2GiB'),
  'media.cellular_profile': s('aac-256'),
  'auth.legacy_subsonic': s(true),
  'discovery.mdns': s(true, 'config', false),
  'backup.schedule': s('daily 03:30'),
  'backup.keep': s(7),
  'uploads.enabled': s(false),
  'acquire.enabled': s(true, 'dashboard'),
  'acquire.library': s(LIB.acquired.id, 'dashboard'),
  'acquire.prefer': s(['ytdlp']),
  'acquire.member_daily': s(50),
  'acquire.member_active': s(5),
  'acquire.global_daily': s(500),
  'acquire.instant_play': s(true, 'dashboard'),
  'acquire.fetched_keep': s(500),
  'acquire.fetched_max_bytes': s('20GiB'),
  'acquire.ytdlp.enabled': s(true, 'dashboard'),
  'enrichment.musicbrainz': s(false),
  'cloud.enabled': s(true),
  'cloud.quota_items': s(2000000),
  'cloud.quota_bytes': s('1GiB'),
  'enrichment.lastfm_api_key': s(true, 'dashboard'),
});
w('admin-server-urls', { urls: [
  { url: 'http://192.168.1.20:4747', kind: 'lan', tls: false, source: 'detected', reachable: true, checked_at: '2026-11-14T09:59:00.000Z' },
  { url: 'http://[fd7a:115c:a1e0::4]:4747', kind: 'lan', tls: false, source: 'detected', reachable: true, checked_at: '2026-11-14T09:59:00.000Z' },
  { url: 'http://home.local:4747', kind: 'lan', tls: false, source: 'detected', reachable: true, checked_at: '2026-11-14T09:59:00.000Z' },
  { url: 'https://home.tail4a2c.ts.net', kind: 'tailnet', tls: true, source: 'configured', reachable: false, checked_at: '2026-11-14T09:59:00.000Z' },
] });

w('admin-backups', { backups: [
  { name: 'trackside-20261114T033000Z.db', kind: 'scheduled', at: '2026-11-14T03:30:00.000Z', bytes: 790000000, has_key: true },
  { name: 'trackside-20261113T033000Z.db', kind: 'scheduled', at: '2026-11-13T03:30:00.000Z', bytes: 786400000, has_key: true },
  { name: 'trackside-20261112T214500Z.db', kind: 'manual', at: '2026-11-12T21:45:00.000Z', bytes: 785100000, has_key: true },
  { name: 'trackside-20261112T033000Z.db', kind: 'scheduled', at: '2026-11-12T03:30:00.000Z', bytes: 784900000, has_key: true },
  { name: 'trackside-20261111T080000Z-pre-migrate-2.db', kind: 'pre_migrate', at: '2026-11-11T08:00:00.000Z', bytes: 781000000, has_key: false },
], next_at: '2026-11-15T03:30:00.000Z' });

w('admin-audit', { entries: [
  { at: '2026-11-14T09:42:10.000Z', actor: 'luke', ip: '192.168.1.41', action: 'pairing.approve', target: 'Living Room Mac', detail: 'macos' },
  { at: '2026-11-14T09:30:58.000Z', actor: 'luke', ip: '192.168.1.41', action: 'acquisition.request', target: 'Halo', detail: 'Beyoncé' },
  { at: '2026-11-14T09:15:20.000Z', actor: 'luke', ip: '192.168.1.41', action: 'invite.create', target: 'member', detail: 'For Nana' },
  { at: '2026-11-14T08:00:05.000Z', actor: 'system', ip: '', action: 'library.unavailable', target: 'NAS Archive', detail: 'The folder is empty or not mounted.' },
  { at: '2026-11-13T21:10:44.000Z', actor: 'sam', ip: '100.90.41.5', action: 'device.create', target: "Sam's iPhone", detail: 'ios' },
  { at: '2026-11-13T18:05:02.000Z', actor: 'luke', ip: '192.168.1.41', action: 'library.scan', target: 'Music', detail: 'full' },
  { at: '2026-11-13T14:00:31.000Z', actor: 'luke', ip: '192.168.1.41', action: 'user.update', target: 'mum', detail: 'can_request on' },
  { at: '2026-11-13T12:31:09.000Z', actor: 'luke', ip: '192.168.1.41', action: 'acquire.provider.update', target: 'ytdlp', detail: 'enabled' },
  { at: '2026-11-12T21:45:00.000Z', actor: 'luke', ip: '192.168.1.41', action: 'backup.create', target: 'trackside-20261112T214500Z.db', detail: 'manual' },
  { at: '2026-11-12T20:02:44.000Z', actor: 'luke', ip: '192.168.1.41', action: 'settings.update', target: 'server.public_urls', detail: '' },
  { at: '2026-11-11T08:00:00.000Z', actor: 'system', ip: '', action: 'server.upgrade', target: '1.0.0', detail: 'schema 2' },
], next_cursor: 'YXVkaXQ6MTE' });

const acq = (o) => ({ quality: 'lossless', existing: false, created_at: '2026-11-14T09:30:00.000Z', updated_at: '2026-11-14T09:30:07.000Z', ...o });
w('admin-acquisitions', { acquisitions: [
  acq({ id: 'q8w1e4r7t0y3u6i9o2p5', state: 'downloading', title: 'Running Up That Hill', artist: 'Kate Bush', album: 'Hounds of Love', progress: { bytes: 3400000, total: 9400000 }, user: ref(mum), provider: 'ytdlp', quality: 'high', candidate: { provider: 'ytdlp', source: 'video wp43OdtAAkM from Kate Bush - Topic', quality: 'high', format: { container: 'm4a', codec: 'aac', bitrate_kbps: 256, sample_rate: 44100 }, size: 9400000 }, attempts: [], created_at: '2026-11-14T09:52:00.000Z', updated_at: '2026-11-14T09:59:41.000Z' }),
  acq({ id: 'q2e5r8t1y4u7i0o3p6a9', state: 'queued', title: 'Teardrop', artist: 'Massive Attack', album: 'Mezzanine', quality: undefined, user: ref(luke), attempts: [], created_at: '2026-11-14T09:58:30.000Z', updated_at: '2026-11-14T09:58:30.000Z' }),
  acq({ id: 'q5t8y1u4i7o0p3a6s9d2', state: 'failed', title: 'Strawberry Letter 23', artist: 'Shuggie Otis', album: 'Freedom Flight', quality: undefined, failure: 'all_candidates_failed', user: ref(mum), attempts: [
    { provider: 'ytdlp', source: 'video Xq1cY8pQf3E from 70sGrooveArchive', at: '2026-11-14T08:12:00.000Z', error: 'Stalled at 41% for 2 minutes.' },
    { provider: 'ytdlp', source: 'video kD7Vb0qQ1xI from Shuggie Otis - Topic', at: '2026-11-14T08:18:30.000Z', error: 'Duration 2:41 does not match 3:58.' },
  ], created_at: '2026-11-14T08:10:00.000Z', updated_at: '2026-11-14T08:18:31.000Z' }),
  acq({ id: 'q9u2i5o8p1a4s7d0f3g6', state: 'done', title: 'Halo', artist: 'Beyoncé', album: 'I Am... Sasha Fierce', song_id: 's2f4g6h8j0k2l4z6x8c0', user: ref(luke), provider: 'ytdlp', candidate: { provider: 'ytdlp', source: 'video bnVUHWCynig from Beyoncé - Topic', quality: 'high', format: { container: 'm4a', codec: 'aac', bitrate_kbps: 256, sample_rate: 44100 }, size: 8400000 }, quality: 'high', attempts: [], created_at: '2026-11-14T09:30:00.000Z', updated_at: '2026-11-14T09:31:13.000Z' }),
] });
w('admin-acquire-providers', { providers });
w('admin-provider-test', { ok: true, elapsed_ms: 1840, candidates: 12, detail: 'Found 12 results for "beyonce halo".' });
w('admin-enrichment', { configured: true, artists: 1840, songs: 5210, last_fetch_at: '2026-11-14T09:28:00.000Z', last_error: { at: '2026-11-14T08:30:00.000Z', message: 'Last.fm error 29: Rate Limit Exceeded' } });
w('admin-enrichment-test', { ok: false, elapsed_ms: 212, detail: 'Last.fm error 10: Invalid API key' });
// GET /admin/apple-music: the key itself is never sent back.
w('admin-apple-music', { configured: true, key_id: 'ABC123DEFG', team_id: 'TEAM456XYZ', set_at: '2026-11-11T09:30:00.000Z' });
w('admin-apple-music-unset', { configured: false });

// Music Assistant (api.md 16): the connection, a test, Trackside's registration, MA's providers.
// The token is never sent back, only token_set.
const MA = 'http://192.168.1.20:8095';
const edit = (id) => `${MA}/#/settings/editprovider/${id}`;
w('admin-music-assistant', {
  auto_register: true, configured: true, connection: { since: '2026-11-14T08:00:00.000Z', state: 'connected' },
  enabled: true, hidden_services: ['tidal--P9wq1Lza'],
  lan_url: { checked_at: '2026-11-14T09:30:00.000Z', reachable: true, source: 'detected', url: 'http://192.168.1.20:4747' },
  relays: { active: 1, max: 4 },
  server: { compatible: true, min_supported_schema_version: 28, name: 'Music Assistant', relay_supported: true, schema_version: 65, server_id: '8f2e4b6a1c3d5e7f9a0b2c4d6e8f0a1b', tested: true, version: '2.10.13' },
  services: 2, source: { token: 'dashboard', url: 'dashboard' }, speakers: 5, token_set: true, token_set_at: '2026-11-12T18:00:00.000Z',
  ui_url: MA, url: MA, user: { display_name: 'Trackside', role: 'admin', username: 'trackside' },
});
// One fixture per connection state, so ?preview=ma-<state> shows each (dev/preview.js).
const maBase = JSON.parse(JSON.stringify({
  auto_register: true, configured: true, enabled: true, hidden_services: ['tidal--P9wq1Lza'],
  lan_url: { checked_at: '2026-11-14T09:30:00.000Z', reachable: true, source: 'detected', url: 'http://192.168.1.20:4747' },
  relays: { active: 1, max: 4 }, source: { token: 'dashboard', url: 'dashboard' }, token_set: true, token_set_at: '2026-11-12T18:00:00.000Z', ui_url: MA, url: MA,
}));
const maServer = { compatible: true, min_supported_schema_version: 28, name: 'Music Assistant', relay_supported: true, schema_version: 65, server_id: '8f2e4b6a1c3d5e7f9a0b2c4d6e8f0a1b', tested: true, version: '2.10.13' };
const maUser = { display_name: 'Trackside', role: 'admin', username: 'trackside' };
const maState = (name, o) => w(`admin-music-assistant-${name}`, { ...maBase, ...o });
maState('connecting', { connection: { state: 'connecting', since: '2026-11-14T09:59:52.000Z' } });
maState('auth-failed', { connection: { state: 'auth_failed', since: '2026-11-14T09:12:00.000Z', last_error: { at: '2026-11-14T09:12:00.000Z', message: 'Music Assistant refused the token (error 23: Invalid or expired token).' } }, server: maServer });
maState('incompatible', { connection: { state: 'incompatible', since: '2026-11-14T09:40:00.000Z' }, server: { ...maServer, compatible: false, schema_version: 52, version: '2.8.4' } });
maState('unreachable', { connection: { state: 'unreachable', since: '2026-11-14T08:47:00.000Z', last_error: { at: '2026-11-14T09:59:30.000Z', message: 'Connection refused at 192.168.1.20:8095.' } }, lan_url: { ...maBase.lan_url, reachable: false } });
maState('setup-required', { connection: { state: 'setup_required', since: '2026-11-14T09:58:00.000Z' }, server: { ...maServer, version: '2.10.13' } });
maState('disabled', { enabled: false, connection: { state: 'disabled' }, server: maServer, user: maUser });
// Set in the config file: address and token are read-only, a private CA is in use, and the
// address Music Assistant uses to reach Trackside is loopback.
maState('config', {
  connection: { state: 'connected', since: '2026-11-14T08:00:00.000Z' }, server: { ...maServer, schema_version: 131, tested: false, version: '2.11.0b14' }, user: maUser,
  custom_ca: true, source: { token: 'config', url: 'config' }, url: 'https://music.home.arpa', ui_url: 'https://music.home.arpa', speakers: 5, services: 2,
  lan_url: { checked_at: '2026-11-14T09:30:00.000Z', reachable: false, source: 'configured', url: 'http://127.0.0.1:4747', warning: 'This address only works on the Trackside machine itself, and Music Assistant runs on another.' },
});
w('admin-music-assistant-unset', { configured: false, connection: { state: 'not_set_up' }, enabled: false, token_set: false });
w('admin-music-assistant-test', { detail: 'Connected to Music Assistant 2.10.13 as trackside (admin).', elapsed_ms: 312, ok: true, players: 5, providers: 9, server: { compatible: true, name: 'Music Assistant', schema_version: 65, tested: true, version: '2.10.13' } });
w('admin-music-assistant-test-failed', { detail: 'Music Assistant refused the token (error 23: Invalid or expired token).', elapsed_ms: 140, ok: false, server: { compatible: true, name: 'Music Assistant', schema_version: 128, tested: true, version: '2.11.0b11' } });
w('admin-music-assistant-registration', {
  base_url: 'http://192.168.1.20:4747',
  key: { created_at: '2026-11-12T18:00:05.000Z', id: 'kwd5w7vf6ptaf3cwd1xbt', last_used_at: '2026-11-14T09:29:00.000Z', prefix: 'tsk_Qm3vXa', scopes: ['library', 'stream'] },
  provider_instance_id: 'opensubsonic--T7rk2mQa', registered_at: '2026-11-12T18:00:06.000Z',
  service_user: { display_name: 'Music Assistant', id: 'uvxvacxf5ak9afxq4qdjj', username: 'music-assistant' },
  state: 'registered', ui_url: edit('opensubsonic--T7rk2mQa'),
});
w('admin-music-assistant-registration-needs-admin', { base_url: 'http://192.168.1.20:4747', last_error: { at: '2026-11-14T09:30:00.000Z', message: 'The Music Assistant account trackside is not an admin, so it cannot add providers.' }, state: 'needs_admin' });
const regBase = { base_url: 'http://192.168.1.20:4747', service_user: { display_name: 'Music Assistant', id: 'uvxvacxf5ak9afxq4qdjj', username: 'music-assistant' } };
w('admin-music-assistant-registration-registering', { ...regBase, state: 'registering' });
w('admin-music-assistant-registration-not-registered', { ...regBase, state: 'not_registered' });
w('admin-music-assistant-registration-failed', { ...regBase, state: 'failed', last_error: { at: '2026-11-14T09:31:00.000Z', message: 'Music Assistant could not reach Trackside at http://192.168.1.20:4747.' } });
w('admin-music-assistant-registration-stale', { ...regBase, state: 'stale', provider_instance_id: 'opensubsonic--T7rk2mQa', registered_at: '2026-11-12T18:00:06.000Z', last_error: { at: '2026-11-14T09:20:00.000Z', message: 'Music Assistant no longer has the Trackside source.' } });
w('admin-music-assistant-providers', {
  music: [
    { available: true, domain: 'spotify', hidden: false, instance_id: 'spotify--Xb4LkP2q', name: 'Spotify', service_id: 'bqtyy4kcexcsg6wa0y4w5', streaming: true, ui_url: edit('spotify--Xb4LkP2q') },
    { available: true, domain: 'tidal', hidden: true, instance_id: 'tidal--P9wq1Lza', name: 'Tidal', service_id: 'b9j3k7m1q5t8w2x6y0z4c', streaming: true, ui_url: edit('tidal--P9wq1Lza') },
    { available: false, domain: 'qobuz', hidden: false, instance_id: 'qobuz--Rv3sM7nE', name: 'Qobuz', service_id: 'c4h8m2q6t0w4y8b2e6j0n', streaming: true, ui_url: edit('qobuz--Rv3sM7nE') },
    { available: true, domain: 'opensubsonic', hidden: false, instance_id: 'opensubsonic--T7rk2mQa', name: 'Trackside (Home)', streaming: false, trackside: true, ui_url: edit('opensubsonic--T7rk2mQa') },
  ],
  players: [
    { available: true, domain: 'sonos', instance_id: 'sonos--Kq2m8VbR', name: 'Sonos', speakers: 3, ui_url: edit('sonos--Kq2m8VbR') },
    { available: true, domain: 'airplay', instance_id: 'airplay--Lz5n1CwT', name: 'AirPlay', speakers: 2, ui_url: edit('airplay--Lz5n1CwT') },
    { available: false, domain: 'chromecast', instance_id: 'chromecast--Jw6p0XsD', name: 'Chromecast', speakers: 0, ui_url: edit('chromecast--Jw6p0XsD') },
  ],
  ui_url: `${MA}/#/settings/providers`,
});

w('me-devices', { devices: devices.filter((d) => d.user.id === luke.id).map(({ user, ...d }) => ({ ...d, current: false })) });
// The member preview signs in as Mum, so her own devices and keys.
w('me-devices-member', { devices: devices.filter((d) => d.user.id === mum.id).map(({ user, ...d }) => ({ ...d, current: false })) });
w('me-api-keys-member', { keys: [] });
w('me-api-keys', { keys: [
  { id: 'k2c4e6g8i0k2m4o6q8s0', kind: 'api_key', name: 'Home Assistant', prefix: 'tsk_Hq7', scopes: ['library'], created_at: '2026-10-01T09:00:00.000Z', last_used_at: '2026-11-14T09:55:00.000Z', last_used_ip: '192.168.1.10' },
] });
w('me-api-key-created', { id: 'k9x1z3c5v7b9n1m3q5w7', kind: 'api_key', name: 'Symfonium', prefix: 'tsk_p3X', scopes: ['library', 'stream', 'write'], created_at: '2026-11-14T10:00:00.000Z', secret: 'tsk_p3XvQ8rL2mN6tY0wB4kD9hJ1sF5gA7cE3uI8oP2zR6x' });
w('me-app-password-created', { id: 'k4v6b8n0m2q4w6e8r0t2', kind: 'app_password', name: 'Feishin', prefix: 'K7MQ', scopes: ['library', 'stream', 'write'], created_at: '2026-11-14T10:00:00.000Z', secret: 'K7MQ-4XD9-P2TA-HN8R-W3CF-6YJB', username: 'luke' });
w('me-pairing-code', { id: 'g4f7h0j3k6l9z2x5c8v1', code: 'Q7RM-4X2K', qr_payload: 'trackside://pair?d=eyJ2IjoxfQ', qr_svg_url: '/api/v1/me/pairing-codes/g4f7h0j3k6l9z2x5c8v1/qr.svg', expires_at: '2026-11-14T10:10:00.000Z' });
w('pair-approved', { device: { name: 'Living Room Mac', platform: 'macos' } });
const tvLink = (state) => ({
  state,
  device: { name: 'Living Room TV', platform: 'android_tv', model: 'Chromecast with Google TV', app_name: 'Tracks' },
  nearby: true,
  requested_at: '2026-11-14T09:58:40.000Z',
  expires_at: '2026-11-14T10:08:40.000Z',
});
w('link-lookup', tvLink('pending'));
w('link-approved', tvLink('approved'));
w('link-denied', tvLink('denied'));
w('user-created', sessionUser);

/* ---------- Stats (see the stats API spec): deterministic, ending 2026-11-14. ---------- */

let seed = 7;
const rand = () => { seed = (seed * 16807) % 2147483647; return (seed - 1) / 2147483646; };
const dayList = (n) => Array.from({ length: n }, (_, i) => new Date(Date.UTC(2026, 10, 14 - (n - 1 - i))).toISOString().slice(0, 10));

const days30 = dayList(30);
const weekend = (d) => [0, 6].includes(new Date(`${d}T12:00:00Z`).getUTCDay());
const plays = (base, spread, joined) => days30.map((d) => (d < joined ? 0 : Math.max(0, Math.round(base * (weekend(d) ? 1.5 : 1) + (rand() - 0.5) * spread))));
const listening = {
  tz: 'Europe/London', bucket: 'day', days: days30,
  users: [
    { id: luke.id, username: 'luke', display_name: 'Luke', plays: plays(46, 30, '2026-01-01') },
    { id: mum.id, username: 'mum', display_name: 'Mum', plays: plays(22, 20, '2026-01-01') },
    { id: sam.id, username: 'sam', display_name: 'Sam Okafor', plays: plays(15, 18, '2026-10-19') },
    { id: grandad.id, username: 'grandad', display_name: 'Grandad', plays: days30.map(() => 0) },
  ],
};
// The server was down for a day: no data recorded, which is not the same as zero plays.
for (const u of listening.users) u.plays[9] = null;
w('admin-stats-listening', listening);

const hours = Array.from({ length: 24 }, (_, i) => new Date(Date.UTC(2026, 10, 13, 11 + i)).toISOString().replace('.000Z', '.000Z'));
const hourOf = (iso) => (new Date(iso).getUTCHours());
const curve = (h) => [0, 0, 0, 0, 0, 0, 1, 2, 2, 1, 1, 1, 2, 2, 1, 1, 2, 3, 4, 5, 5, 4, 2, 1][h];
w('admin-stats-streams', {
  bucket: 'hour', hours,
  streams: hours.map((h) => curve(hourOf(h)) + (rand() > 0.7 ? 1 : 0)),
  transcodes: hours.map((h) => Math.max(0, curve(hourOf(h)) - 2 + (rand() > 0.8 ? 1 : 0))),
  failed: hours.map((h) => (hourOf(h) === 8 ? 6 : hourOf(h) === 9 ? 2 : 0)),
  transcode_limit: 4,
});

const days90 = dayList(90);
w('admin-stats-storage', {
  bucket: 'day', days: days90,
  // The last day matches the overview's data_free_bytes, so the page shows one number for it.
  data_free_bytes: days90.map((_, i) => (i === 89 ? overview.storage.data_free_bytes : Math.round(150e9 - i * 0.33e9 + (rand() - 0.5) * 1.2e9))),
  database_bytes: days90.map((_, i) => Math.round(700e6 + i * 1.25e6)),
  cache_bytes: days90.map((_, i) => Math.round(2.2e9 + i * 11e6 + (rand() - 0.5) * 2e8)),
  volume_bytes: 500e9,
});

w('admin-stats-requests', {
  tz: 'Europe/London', bucket: 'day', days: days30,
  added: days30.map((d) => (d < '2026-10-24' ? 0 : Math.round(rand() * 5 + (weekend(d) ? 3 : 0)))),
  failed: days30.map((d) => (d < '2026-10-24' ? 0 : rand() > 0.6 ? 1 : 0)),
  cancelled: days30.map(() => (rand() > 0.9 ? 1 : 0)),
});
