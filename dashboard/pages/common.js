// Small pieces several pages share.

import { h, badge, icon, plural } from '../ui.js';

export const LIBRARY_STATE = {
  ok: ['Online', 'good'],
  unavailable: ['Offline', 'warn'],
};

export function libraryBadge(library) {
  if (library.scanning) return h('span.badge.accent', h('span.dot.live'), 'Scanning');
  if (library.enabled === false) return badge('Paused');
  const [text, kind] = LIBRARY_STATE[library.state] || [library.state, ''];
  return library.state === 'ok' ? null : badge(text, kind);
}

export function songs(n) {
  return plural(n, 'song');
}

const SCAN_KIND = { quick: 'Quick scan', full: 'Full scan' };
export const scanKind = (k) => SCAN_KIND[k] || 'Scan';

/** "212 added, 3 moved, 1 missing" or "No changes". */
export function scanSummary(run) {
  const parts = [];
  if (run.added) parts.push(`${run.added.toLocaleString()} added`);
  if (run.updated) parts.push(`${run.updated.toLocaleString()} updated`);
  if (run.moved) parts.push(`${run.moved.toLocaleString()} moved`);
  if (run.missing) parts.push(`${run.missing.toLocaleString()} missing`);
  const errors = Array.isArray(run.errors) ? run.errors.length : run.errors;
  if (errors) parts.push(plural(errors, 'error'));
  return parts.length ? parts.join(', ') : 'No changes';
}

const ROLE = { admin: 'Admin', member: 'Member' };
export const roleName = (r) => ROLE[r] || r;

/** [key, label, sub, on when the server leaves it out]. */
export const PERMISSIONS = [
  ['can_download', 'Download songs', 'Keep songs on their phone to play offline', false],
  ['can_upload', 'Upload songs', 'Add their own files to the uploads library', false],
  ['can_request', 'Request songs', 'Ask Trackside to find songs the library does not have', false],
  ['legacy_subsonic', 'Use older Subsonic apps', 'Make app passwords for apps like Symfonium or Feishin', false],
  ['can_use_speakers', 'Play on speakers', 'Speakers around the home, through Music Assistant', true],
  ['can_play_services', 'Play streaming services', 'Spotify, Tidal and the others added to Music Assistant', true],
];

/** Music Assistant's connection state: a dot and a few words. */
export const ASSISTANT_STATE = {
  not_set_up: ['', 'Not set up'],
  disabled: ['', 'Turned off'],
  connecting: ['live', 'Connecting'],
  connected: ['good', 'Connected'],
  unreachable: ['warn', 'Cannot reach Music Assistant'],
  auth_failed: ['bad', 'Token refused'],
  incompatible: ['warn', 'Version not supported'],
  setup_required: ['warn', 'Music Assistant needs setting up'],
};
export const assistantState = (state) => ASSISTANT_STATE[state] || ['warn', state || 'Unknown'];

/** A link into Music Assistant's own interface, only for a web address from the server. */
export function assistantHref(url) {
  return typeof url === 'string' && /^https?:\/\//i.test(url) ? url : null;
}

export function warnIcon(severity) {
  if (severity === 'critical' || severity === 'error') return icon('critical', 'tint-bad');
  if (severity === 'info') return icon('info', 'tint-muted');
  return icon('warning', 'tint-warn');
}
