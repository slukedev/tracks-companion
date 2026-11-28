// Answers api.js calls from dev/fixtures when the page is opened with ?preview.
// Variants: ?preview=member, noacq, nostats, fresh, setup, signedout, ratelimit, slow, error, reauth,
// Music Assistant: ma-connecting, ma-auth-failed, ma-incompatible, ma-unreachable, ma-setup-required,
// ma-disabled, ma-config (address and token set in the config file); and Trackside's registration in
// it: needsadmin, reg-registering, reg-failed, reg-stale, reg-not-registered. A test or a saved
// token "wrong" is refused; a test of an address with "nowhere" in it cannot connect. Connecting and
// registering finish by themselves after a moment.
// A pasted Apple Music key without BEGIN PRIVATE KEY answers 400, to see the field error.
// Connect a TV: a code starting 0 is unknown, E expired, D used, K locked, R rate limited,
// N from outside the home network; any other code is a TV waiting.

import { ApiError } from '../api.js';

const cache = new Map();
async function fixture(name) {
  if (!cache.has(name)) {
    cache.set(name, fetch(new URL(`fixtures/${name}.json`, import.meta.url)).then((r) => {
      if (!r.ok) throw new Error(`missing fixture ${name}`);
      return r.json();
    }));
  }
  return structuredClone(await cache.get(name));
}

const fail = (status, code, message, details) => { throw new ApiError(status, code, message, details); };
const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

let signedIn = sessionStorage.getItem('preview-signed-in') === '1';
let reauthed = false;
const SENSITIVE = ['PATCH /admin/users/{id}', 'DELETE /admin/users/{id}', 'POST /me/api-keys', 'POST /admin/users/{id}/password-reset', 'PUT /admin/apple-music', 'DELETE /admin/apple-music', 'DELETE /admin/music-assistant', 'POST /admin/music-assistant/registration', 'DELETE /admin/music-assistant/registration'];
// The Apple Music key: saved in the normal preview, absent on a fresh server, and changed by the page.
let appleMusicSet = null;
// Music Assistant: connected in the normal preview, not set up on a fresh server, and changed
// by the page. A connecting state turns into connected at maReadyAt, registering at regReadyAt.
let ma = null;
let maReadyAt = 0;
let reg = null;
let regReadyAt = 0;
const PREVIEW_NOW = '2026-11-14T10:00:00.000Z';
const MA_VARIANTS = ['ma-connecting', 'ma-auth-failed', 'ma-incompatible', 'ma-unreachable', 'ma-setup-required', 'ma-disabled', 'ma-config'];
const REG_VARIANTS = { needsadmin: 'needs-admin', 'reg-registering': 'registering', 'reg-failed': 'failed', 'reg-stale': 'stale', 'reg-not-registered': 'not-registered' };

async function assistant(variant) {
  if (!ma) {
    ma = await fixture(['fresh', 'noacq'].includes(variant) ? 'admin-music-assistant-unset'
      : MA_VARIANTS.includes(variant) ? `admin-music-assistant-${variant.slice(3)}` : 'admin-music-assistant');
    if (variant === 'needsadmin' && ma.user) ma.user.role = 'user';
  }
  if (maReadyAt && Date.now() >= maReadyAt) {
    maReadyAt = 0;
    const connected = await fixture('admin-music-assistant');
    ma = { ...ma, connection: connected.connection, server: ma.server || connected.server, user: ma.user || connected.user, speakers: connected.speakers, services: connected.services };
  }
  return structuredClone(ma);
}

async function registration(variant) {
  const conn = await assistant(variant);
  if (!conn.configured) return fixture('admin-music-assistant-registration-not-registered');
  if (!reg) reg = await fixture(REG_VARIANTS[variant] ? `admin-music-assistant-registration-${REG_VARIANTS[variant]}` : 'admin-music-assistant-registration');
  if (regReadyAt && Date.now() >= regReadyAt) { regReadyAt = 0; reg = await fixture('admin-music-assistant-registration'); }
  return structuredClone(reg);
}

async function patchAssistant(variant, body = {}) {
  const current = await assistant(variant);
  for (const key of ['url', 'token']) {
    if (key in body && current.source?.[key] === 'config') fail(409, 'conflict', '', { reason: 'set_by_config', field: key });
  }
  if (typeof body.url === 'string' && !/^[a-z][a-z0-9+.-]*:\/\/[^/\s]+/i.test(body.url)) fail(400, 'bad_request', 'Use the whole address, as your browser shows it.', { field: 'url' });
  if (typeof body.lan_url === 'string' && !/^[a-z][a-z0-9+.-]*:\/\/[^/\s]+/i.test(body.lan_url)) fail(400, 'bad_request', 'Use the whole address, as your browser shows it.', { field: 'lan_url' });
  if (typeof body.ca_pem === 'string' && !/BEGIN CERTIFICATE/.test(body.ca_pem)) fail(400, 'bad_request', 'That is not a PEM certificate. It starts with BEGIN CERTIFICATE.', { field: 'ca_pem' });
  const next = current.configured ? current : { ...(await fixture('admin-music-assistant')), url: undefined, ui_url: undefined, token_set: false };
  if (typeof body.url === 'string') { next.url = body.url; next.ui_url = body.url; }
  if (typeof body.token === 'string') { next.token_set = true; next.token_set_at = PREVIEW_NOW; }
  if (body.token === null) { next.token_set = false; delete next.token_set_at; }
  if (body.lan_url === null) next.lan_url = (await fixture('admin-music-assistant')).lan_url;
  else if (typeof body.lan_url === 'string') next.lan_url = { url: body.lan_url, source: 'configured', reachable: false };
  if ('ca_pem' in body) next.custom_ca = body.ca_pem !== null;
  if (Array.isArray(body.hidden_services)) next.hidden_services = body.hidden_services;
  if (typeof body.auto_register === 'boolean') next.auto_register = body.auto_register;
  if (typeof body.relay_max_sessions === 'number') next.relays = { ...(next.relays || { active: 0 }), max: body.relay_max_sessions };
  next.configured = true;
  const reconnect = 'url' in body || 'token' in body || 'ca_pem' in body || body.enabled === true || !current.configured;
  if (typeof body.enabled === 'boolean') next.enabled = body.enabled;
  if (body.enabled === false) { next.connection = { state: 'disabled' }; maReadyAt = 0; }
  else if (body.token === 'wrong') { next.connection = (await fixture('admin-music-assistant-auth-failed')).connection; maReadyAt = 0; }
  else if (reconnect) { next.connection = { state: 'connecting', since: PREVIEW_NOW }; maReadyAt = Date.now() + 2500; }
  ma = next;
  return structuredClone(ma);
}

async function assistantProviders(variant) {
  const conn = await assistant(variant);
  if (conn.connection.state !== 'connected') fail(503, 'catalog_unavailable', 'Music Assistant is unavailable.');
  const lists = await fixture('admin-music-assistant-providers');
  for (const m of lists.music) m.hidden = (conn.hidden_services || []).includes(m.instance_id);
  return lists;
}

// The overview's music_assistant block and problems, as the server words them.
const MA_PROBLEMS = {
  unreachable: ['music_assistant_unreachable', 'Music Assistant is unreachable', 'Trackside cannot reach Music Assistant, so speakers and music services are unavailable.', 'Check that Music Assistant runs and that its address under Speakers is right.'],
  auth_failed: ['music_assistant_auth_failed', 'Music Assistant refused the token', 'Music Assistant does not accept the saved token.', 'Create a long-lived token in Music Assistant (Settings, Profile) and save it under Speakers.'],
  incompatible: ['music_assistant_incompatible', 'Music Assistant version unsupported', 'This Music Assistant version does not work with Trackside.', 'Trackside needs Music Assistant 2.10 or newer.'],
};
const REG_PROBLEM = {
  needs_admin: "The Music Assistant token is not an admin's, so Trackside cannot add itself as a source.",
  stale: "Music Assistant no longer has Trackside's source.",
  failed: 'Trackside could not add itself to Music Assistant as a source.',
};
async function withAssistant(overview, variant) {
  const conn = await assistant(variant);
  const state = conn.connection.state;
  if (!conn.configured) { overview.music_assistant = { state: 'not_set_up' }; return overview; }
  const r = await registration(variant);
  overview.music_assistant = { state, registration: r.state, relays: conn.relays?.active ?? 0, services: conn.services, speakers: conn.speakers };
  const problem = (code, title, message, help) => ({ severity: 'warning', code, title, message, help });
  if (MA_PROBLEMS[state]) overview.problems.push(problem(...MA_PROBLEMS[state]));
  if (REG_PROBLEM[r.state]) overview.problems.push(problem('music_assistant_registration', 'Library not on speakers', REG_PROBLEM[r.state], 'Open Speakers in the dashboard and register again.'));
  return overview;
}

const reads = {
  'GET /setup/status': (v) => (v === 'setup' ? 'setup-status-required' : 'setup-status'),
  'GET /capabilities': () => 'capabilities',
  'GET /readyz': () => 'readyz',
  'GET /me': () => 'me',
  'GET /admin/overview': (v) => ({ noacq: 'admin-overview-noacq', fresh: 'admin-overview-fresh' }[v] || 'admin-overview'),
  'GET /admin/users': () => 'admin-users',
  'GET /admin/users/{id}': () => 'admin-user',
  'GET /admin/invites': () => 'admin-invites',
  'GET /admin/libraries': () => 'admin-libraries',
  'GET /admin/libraries/{id}/scans': (_, values) => ({ l7c2d9f4a1e8b6m3n5k0: 'admin-library-scans-nas', l3m9q2w8e5r1t7y4u6i0: 'admin-library-scans-running' }[values.id] || 'admin-library-scans'),
  'GET /admin/scans/{id}': () => 'admin-scan',
  'GET /admin/libraries/{id}/problems': () => 'admin-library-problems',
  'GET /admin/duplicates': () => 'admin-duplicates',
  'GET /admin/imports': () => 'admin-imports',
  'GET /admin/devices': () => 'admin-devices',
  'GET /admin/api-keys': () => 'admin-api-keys',
  'GET /admin/sessions': () => 'admin-sessions',
  'GET /admin/settings': () => 'admin-settings',
  'GET /admin/enrichment': () => 'admin-enrichment',
  'GET /admin/apple-music': (v) => ((appleMusicSet ?? v !== 'fresh') ? 'admin-apple-music' : 'admin-apple-music-unset'),
  // Answered by assistant(), registration() and assistantProviders() below.
  'GET /admin/music-assistant': () => null,
  'GET /admin/music-assistant/registration': () => null,
  'GET /admin/music-assistant/providers': () => null,
  'GET /admin/server/urls': () => 'admin-server-urls',
  'GET /admin/backups': () => 'admin-backups',
  'GET /admin/audit': () => 'admin-audit',
  'GET /admin/acquire/providers': () => 'admin-acquire-providers',
  'GET /admin/acquisitions': () => 'admin-acquisitions',
  'GET /admin/stats/listening': () => 'admin-stats-listening',
  'GET /admin/stats/streams': () => 'admin-stats-streams',
  'GET /admin/stats/storage': () => 'admin-stats-storage',
  'GET /admin/stats/requests': () => 'admin-stats-requests',
  'GET /me/devices': (v) => (v === 'member' ? 'me-devices-member' : 'me-devices'),
  'GET /me/api-keys': (v) => (v === 'member' ? 'me-api-keys-member' : 'me-api-keys'),
  'GET /invites/{token}': () => 'invite-info',
  'GET /password-resets/{token}': () => null,
};

const writes = {
  'POST /auth/session': () => { signedIn = true; sessionStorage.setItem('preview-signed-in', '1'); return 'auth-session'; },
  'DELETE /auth/session': () => { signedIn = false; sessionStorage.removeItem('preview-signed-in'); return null; },
  'POST /auth/reauth': (_, __, body) => { if (body?.password === 'wrong') fail(401, 'unauthenticated', ''); reauthed = true; return null; },
  'POST /setup': () => 'user-created',
  'POST /invites/{token}/accept': () => 'user-created',
  'POST /password-resets/{token}': () => null,
  'POST /admin/users': () => 'admin-user-created',
  'POST /admin/users/{id}/password-reset': () => 'admin-password-reset',
  'POST /admin/invites': () => 'admin-invite-created',
  'POST /admin/libraries': () => 'admin-library-created',
  'POST /admin/libraries/{id}/scan': () => 'admin-scan-started',
  'PUT /admin/settings/targets': () => 'admin-targets',
  'POST /admin/pairing/approve': () => 'admin-pairing-approved',
  'POST /pair/requests/approve': () => 'pair-approved',
  'POST /admin/acquire/providers/{name}/test': () => 'admin-provider-test',
  'POST /admin/enrichment/test': () => 'admin-enrichment-test',
  'POST /me/pairing-codes': () => 'me-pairing-code',
  'PUT /admin/apple-music': (_, __, body) => {
    if (!/BEGIN PRIVATE KEY/.test(body?.private_key || '')) fail(400, 'bad_request', 'private_key is not a P-256 private key in PKCS#8 form. Use the .p8 file from Apple.', { field: 'private_key' });
    appleMusicSet = true;
    return 'admin-apple-music';
  },
  'DELETE /admin/apple-music': () => { appleMusicSet = false; return null; },
  'DELETE /admin/music-assistant': () => { maReadyAt = 0; regReadyAt = 0; reg = null; ma = { configured: false, connection: { state: 'not_set_up' }, enabled: false, token_set: false }; return null; },
  'DELETE /admin/music-assistant/registration': () => { regReadyAt = 0; reg = { state: 'not_registered' }; return null; },
  'PATCH /admin/settings': (_, __, body) => {
    if (body?.['acquire.fetched_keep'] < 0) fail(400, 'bad_request', 'Use 0 or more.', { field: 'acquire.fetched_keep' });
    if (typeof body?.['acquire.fetched_max_bytes'] === 'string' && !/^\d+(\.\d+)?\s*[KMGT]i?B$/i.test(body['acquire.fetched_max_bytes'])) fail(400, 'bad_request', 'Write a size like 20GiB.', { field: 'acquire.fetched_max_bytes' });
    return null;
  },
  'POST /me/api-keys': (_, __, body) => (body?.kind === 'app_password' ? 'me-app-password-created' : 'me-api-key-created'),
};

/** Connect a TV: the first character of the code picks what the server would say. */
async function linkAnswer(name, body) {
  const code = String(body?.user_code || '').toUpperCase();
  const first = code.charAt(0);
  if (first === '0') fail(404, 'not_found', '');
  if (first === 'E') fail(410, 'link_expired', '');
  if (first === 'D') fail(409, 'conflict', '', { reason: 'link_used' });
  if (first === 'K') fail(410, 'link_locked', '');
  if (first === 'R') { const e = new ApiError(429, 'rate_limited', ''); e.retryAfter = 90; throw e; }
  const link = await fixture(name);
  if (first === 'N') link.nearby = false;
  return link;
}

export async function answer(route, { values = {}, body, query = {}, variant }) {
  await wait(variant === 'slow' ? 1500 : 120);
  const [method] = route.split(' ');
  if (route === 'GET /auth/session') {
    if ((variant === 'signedout' || variant === 'ratelimit') && !signedIn) fail(401, 'unauthenticated', 'Sign in.');
    return fixture(variant === 'member' ? 'auth-session-member' : 'auth-session');
  }
  if (variant === 'error' && route === 'GET /admin/overview') fail(500, 'internal', '');
  if (variant === 'reauth' && SENSITIVE.includes(route) && !reauthed) fail(403, 'reauth_required', '');
  if (variant === 'ratelimit' && route === 'POST /auth/session') { const e = new ApiError(429, 'rate_limited', ''); e.retryAfter = 42; throw e; }
  if (variant === 'member' && route.includes(' /admin/')) fail(403, 'forbidden', '');
  if (variant === 'noacq' && /\/admin\/(acquire|acquisitions|stats\/requests)/.test(route)) fail(404, 'not_found', '');
  if ((variant === 'nostats' || variant === 'fresh') && route.startsWith('GET /admin/stats/')) fail(404, 'not_found', '');
  if (variant === 'fresh') {
    const empty = {
      'GET /admin/users': async () => ({ users: (await fixture('admin-users')).users.slice(0, 1) }),
      'GET /admin/libraries': () => ({ libraries: [] }),
      'GET /admin/invites': () => ({ invites: [] }),
      'GET /admin/devices': () => ({ devices: [] }),
      'GET /admin/api-keys': () => ({ keys: [] }),
      'GET /admin/backups': () => ({ backups: [] }),
      'GET /admin/audit': () => ({ entries: [] }),
      'GET /me/devices': () => ({ devices: [] }),
      'GET /me/api-keys': () => ({ keys: [] }),
      'GET /admin/imports': () => ({ imports: [] }),
      'GET /admin/duplicates': () => ({ groups: [] }),
    }[route];
    if (empty) return empty();
  }
  if (route === 'GET /admin/users/{id}' && values.id) {
    const { users } = await fixture('admin-users');
    const base = await fixture('admin-user');
    const user = users.find((u) => u.id === values.id);
    if (!user) fail(404, 'not_found', '');
    return user.id === base.id ? base : { ...user, devices: 0, api_keys: 0, sessions: 0 };
  }
  if (route === 'GET /admin/imports') {
    if (query.cursor) return { imports: [] };
    const page = await fixture('admin-imports');
    if (query.state) { page.imports = page.imports.filter((i) => i.state === query.state); delete page.next_cursor; }
    return page;
  }
  if (route === 'GET /admin/audit') {
    if (query.cursor) return { entries: [] };
    const page = await fixture('admin-audit');
    if (query.action) page.entries = page.entries.filter((e) => e.action.startsWith(query.action));
    if (query.actor) page.entries = page.entries.filter((e) => e.actor === query.actor);
    if (query.action || query.actor) delete page.next_cursor;
    return page;
  }
  if (variant === 'reauth' && route === 'PATCH /admin/music-assistant' && body && ('token' in body || 'url' in body) && !reauthed) fail(403, 'reauth_required', '');
  if (route === 'GET /admin/music-assistant') return assistant(variant);
  if (route === 'PATCH /admin/music-assistant') return patchAssistant(variant, body);
  if (route === 'GET /admin/music-assistant/registration') return registration(variant);
  if (route === 'GET /admin/music-assistant/providers') return assistantProviders(variant);
  if (route === 'POST /admin/music-assistant/registration') {
    reg = await fixture('admin-music-assistant-registration-registering');
    regReadyAt = Date.now() + 2500;
    return structuredClone(reg);
  }
  if (route === 'POST /admin/music-assistant/test') {
    if (body?.token === 'wrong') return fixture('admin-music-assistant-test-failed');
    if (/nowhere/.test(body?.url || '')) return { ok: false, elapsed_ms: 3004, detail: 'Could not connect: no answer within 3 seconds.' };
    return fixture('admin-music-assistant-test');
  }
  if (route === 'GET /admin/overview' && variant !== 'error') {
    const name = reads[route](variant);
    return withAssistant(await fixture(name), variant);
  }
  if (route === 'POST /link/lookup') return linkAnswer('link-lookup', body);
  if (route === 'POST /link/approve') return linkAnswer('link-approved', body);
  if (route === 'POST /link/deny') return linkAnswer('link-denied', body);
  if (route.startsWith('GET /admin/stats/') && !(variant === 'nostats' || variant === 'fresh')) return sliceStats(await fixture(reads[route]()), query);
  const table = method === 'GET' ? reads : writes;
  const pick = table[route];
  if (!pick) {
    if (method === 'GET') throw new Error(`no preview fixture for ${route}`);
    return null;
  }
  const name = pick(variant, values, body);
  return name ? fixture(name) : null;
}

/** The fixtures hold the longest range; a shorter one is the tail. A longer one repeats nothing. */
function sliceStats(data, query) {
  const n = Number(query.days || query.hours);
  const key = data.days ? 'days' : 'hours';
  if (!n || n >= data[key].length) return data;
  const from = data[key].length - n;
  for (const [k, v] of Object.entries(data)) {
    if (Array.isArray(v) && v.length === data[key].length) data[k] = v.slice(from);
  }
  for (const u of data.users || []) u.plays = u.plays.slice(from);
  return data;
}
