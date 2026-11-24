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

export const PERMISSIONS = [
  ['can_download', 'Download songs', 'Keep songs on their phone to play offline'],
  ['can_upload', 'Upload songs', 'Add their own files to the uploads library'],
  ['can_request', 'Request songs', 'Ask Trackside to find songs the library does not have'],
  ['legacy_subsonic', 'Use older Subsonic apps', 'Make app passwords for apps like Symfonium or Feishin'],
];

export function warnIcon(severity) {
  if (severity === 'critical' || severity === 'error') return icon('critical', 'tint-bad');
  if (severity === 'info') return icon('info', 'tint-muted');
  return icon('warning', 'tint-warn');
}
