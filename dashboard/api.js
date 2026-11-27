// The only module that talks to the server. Routes are written as 'METHOD /path/{param}'
// templates relative to /api/v1; dev/check-coverage.mjs reads these strings.

const apiBase = new URL('api/v1/', import.meta.url);
const params = new URLSearchParams(location.search);

export const preview = params.has('preview');
export const previewVariant = params.get('preview') || '';

let previewModule = null;
async function previewHandler() {
  if (!previewModule) previewModule = await import('./dev/preview.js');
  return previewModule;
}

/** The current time; fixed in preview so fixtures read the same on any day. */
export function now() {
  return preview ? Date.parse('2026-11-14T10:00:00.000Z') : Date.now();
}

export class ApiError extends Error {
  constructor(status, code, message, details = {}, requestId = '') {
    super(message);
    this.status = status;
    this.code = code;
    this.details = details;
    this.requestId = requestId;
  }
}

const messages = {
  bad_request: 'Something in the form needs another look.',
  unauthenticated: 'Your session ended. Sign in again.',
  forbidden: 'Your account cannot do that.',
  reauth_required: 'Confirm your password to continue.',
  csrf: 'The page is out of date. Reload it and try again.',
  setup_required: 'Set up the server first.',
  not_found: 'It is no longer there. It may have been removed.',
  conflict: 'That already exists.',
  idempotency_mismatch: 'Reload the page and try again.',
  precondition_failed: 'Someone changed this at the same time. Reload to see their change.',
  too_large: 'That is too large for the server.',
  invalid_state: 'It can no longer be changed.',
  rate_limited: 'Too many tries. Wait a moment, then try again.',
  internal: 'The server ran into a problem.',
  library_unavailable: 'That library is offline.',
  unavailable: 'The server is busy.',
  insufficient_storage: 'The server is out of disk space.',
  network: 'Cannot reach the server.',
  catalog_unavailable: 'Music Assistant is not answering right now.',
};

const reasons = {
  set_by_config: 'This is set in the config file, so it cannot be changed here.',
  last_admin: 'This is the only admin. Make someone else an admin first.',
  username_taken: 'That username is taken.',
  running: 'A backup is already running.',
  service_account: 'This account belongs to Music Assistant and never signs in.',
};

/** A message fit to show a person, preferring what the server said about a bad field. */
export function describe(error) {
  if (!(error instanceof ApiError)) return messages.internal;
  const reason = error.details?.reason;
  if (reason && reasons[reason]) return reasons[reason];
  if (error.code === 'bad_request' && error.message) return error.message;
  if (error.code === 'conflict' && error.message && !reason) return error.message;
  if (error.code === 'rate_limited' && error.retryAfter) {
    return `Too many tries. Try again in ${error.retryAfter} seconds.`;
  }
  return messages[error.code] || error.message || messages.internal;
}

function fill(path, values) {
  return path.replace(/\{(\w+)\}/g, (_, key) => {
    if (values[key] === undefined) throw new Error(`missing route value ${key}`);
    return encodeURIComponent(values[key]);
  });
}

function split(route) {
  const space = route.indexOf(' ');
  return [route.slice(0, space), route.slice(space + 1)];
}

/** An absolute URL for a route, for links, images and downloads. */
// Health routes live at the server root, next to /api/v1 rather than under it.
const ROOT_ROUTES = /^\/(readyz|healthz)$/;

export function href(route, values = {}, query) {
  const [, path] = split(route);
  if (preview) return previewHref(route, values);
  const url = new URL(`${ROOT_ROUTES.test(path) ? '../../' : ''}${fill(path, values).slice(1)}`, apiBase);
  if (query) for (const [k, v] of Object.entries(query)) if (v !== undefined && v !== '') url.searchParams.set(k, v);
  return url.href;
}

/** Server-relative URLs such as qr_svg_url, made absolute against the API base so base_path works. */
export function serverUrl(path) {
  if (preview) return new URL(path.endsWith('qr.svg') && path.includes('invites') ? 'dev/fixtures/invite-qr.svg' : 'dev/fixtures/pairing-qr.svg', import.meta.url).href;
  const rel = path.replace(/^\/api\/v1\//, '');
  return new URL(rel, apiBase).href;
}

function previewHref(route, values) {
  if (route.includes('/backups/')) return new URL('dev/fixtures/admin-backups.json', import.meta.url).href;
  if (route.includes('/export')) return new URL('dev/fixtures/me-devices.json', import.meta.url).href;
  return '#';
}

const signedOutListeners = new Set();
export function onSignedOut(fn) { signedOutListeners.add(fn); }

// Sensitive actions (roles, keys, deleting people, backups) may answer 403 reauth_required.
// The shell registers a password prompt here; the request is retried once after it succeeds.
let reauthPrompt = null;
export function onReauthRequired(fn) { reauthPrompt = fn; }

/** Before a sensitive action that is a plain link (a backup download), make sure the sign-in is recent. */
export async function ensureRecentAuth() {
  const session = await call('GET /auth/session', { quietAuth: true });
  if (!session?.recent_auth_until) return true;
  if (Date.parse(session.recent_auth_until) > now()) return true;
  return reauthPrompt ? reauthPrompt() : false;
}

/**
 * Calls a route. `values` fills {params}; `body` is sent as JSON; `query` adds search params.
 * Resolves to parsed JSON (or null for 204). Rejects with ApiError.
 */
export async function call(route, options = {}) {
  try {
    return await send(route, options);
  } catch (error) {
    if (error.code === 'reauth_required' && reauthPrompt && !options.retried && await reauthPrompt()) {
      return send(route, { ...options, retried: true });
    }
    throw error;
  }
}

async function send(route, { values = {}, body, query, signal, quietAuth = false } = {}) {
  const [method] = split(route);
  if (preview) {
    const { answer } = await previewHandler();
    return answer(route, { values, body, query, variant: previewVariant });
  }
  const headers = { Accept: 'application/json' };
  if (method !== 'GET') headers['X-Trackside-CSRF'] = '1';
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  let response;
  try {
    response = await fetch(href(route, values, query), {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      credentials: 'same-origin',
      cache: 'no-store',
      signal,
    });
  } catch (cause) {
    if (cause?.name === 'AbortError') throw cause;
    throw new ApiError(0, 'network', messages.network);
  }
  if (response.status === 204) return null;
  let data = null;
  const text = await response.text();
  if (text) {
    try { data = JSON.parse(text); } catch { data = null; }
  }
  if (!response.ok) {
    const err = data?.error || {};
    const error = new ApiError(response.status, err.code || (response.status >= 500 ? 'internal' : 'bad_request'), err.message || '', err.details || {}, err.request_id || '');
    const retry = Number(response.headers.get('Retry-After'));
    if (retry > 0) error.retryAfter = retry;
    if (error.code === 'unauthenticated' && !quietAuth) signedOutListeners.forEach((fn) => fn());
    throw error;
  }
  return data;
}

export const get = (route, options) => call(route, options);
