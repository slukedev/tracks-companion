// Audit log entries written as plain sentences, for Overview and Activity.

const SENTENCES = {
  'pairing.approve': ['devices', 'indigo', (a, e) => `${a} added ${e.target}`],
  'device.create': ['devices', 'indigo', (a, e) => `${a} signed in on ${e.target}`],
  'device.revoke': ['devices', 'indigo', (a, e) => `${a} removed ${e.target}`],
  'key.create': ['key', 'indigo', (a, e) => `${a} created the key ${e.target}`],
  'key.revoke': ['key', 'indigo', (a, e) => `${a} revoked the key ${e.target}`],
  'invite.create': ['invite', 'pink', (a, e) => `${a} created an invite${e.detail ? `: ${e.detail}` : ''}`],
  'invite.revoke': ['invite', 'pink', (a) => `${a} revoked an invite`],
  'invite.accept': ['person-plus', 'pink', (a, e) => `${e.target} joined`],
  'user.create': ['person-plus', 'pink', (a, e) => `${a} added ${e.target}`],
  'user.update': ['users', 'pink', (a, e) => `${a} changed what ${e.target} can do`],
  'user.delete': ['users', 'pink', (a, e) => `${a} removed ${e.target}`],
  'user.password_reset': ['lock', 'pink', (a, e) => `${a} made a password reset link for ${e.target}`],
  'user.sign_out': ['users', 'pink', (a, e) => `${a} signed ${e.target} out`],
  'library.create': ['library', 'blue', (a, e) => `${a} added the library ${e.target}`],
  'library.update': ['library', 'blue', (a, e) => `${a} changed ${e.target}`],
  'library.delete': ['library', 'blue', (a, e) => `${a} removed the library ${e.target}`],
  'library.scan': ['scan', 'blue', (a, e) => `${a} started a ${e.detail === 'full' ? 'full' : 'quick'} scan of ${e.target}`],
  'library.unavailable': ['warning', 'warn', (a, e) => `${e.target} went offline`],
  'library.available': ['library', 'blue', (a, e) => `${e.target} is back online`],
  'backup.create': ['backups', 'teal', (a) => `${a} made a backup`],
  'backup.delete': ['backups', 'teal', (a, e) => `${a} deleted the backup ${e.target}`],
  'settings.update': ['settings', 'muted', (a) => `${a} changed settings`],
  'server.upgrade': ['overview', 'muted', (a, e) => `Trackside updated to ${e.target}`],
  'acquisition.request': ['requests', 'accent', (a, e) => `${a} requested ${e.target}${e.detail ? ` by ${e.detail}` : ''}`],
};

/**
 * One audit entry as a sentence. Actions about requests never show their target or detail,
 * because those can name the sources, which only the Requests page shows.
 */
export function sentence(entry, names) {
  const actor = entry.actor === 'system' || !entry.actor ? 'Trackside' : names.get(entry.actor) || entry.actor;
  entry = { ...entry, target: names.get(entry.target) || entry.target };
  const known = SENTENCES[entry.action];
  if (known) return { icon: known[0], tint: known[1], text: known[2](actor, entry) };
  if (/^acqui/.test(entry.action)) return { icon: 'requests', tint: 'accent', text: `${actor} changed how requests work` };
  const verb = entry.action.split('.').reverse().join(' ').replace(/_/g, ' ');
  return { icon: 'activity', tint: 'muted', text: `${actor}: ${verb}${entry.target ? ` ${entry.target}` : ''}` };
}
