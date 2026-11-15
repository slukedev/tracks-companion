// Role, libraries and permissions: shared by Users and Invites.

import { h, segmented, checkbox, select, field, nextId } from '../ui.js';
import { PERMISSIONS } from './common.js';

const BITRATES = [['', 'No limit'], ['320', '320 kbps'], ['256', '256 kbps'], ['192', '192 kbps'], ['128', '128 kbps']];

/**
 * value: { role, permissions, library_ids }. Libraries come from GET /admin/libraries.
 * Admins see every library, so the library list hides while Admin is chosen.
 */
export function accessForm(value, libraries, { acquisition }) {
  let role = value.role || 'member';
  const name = nextId('role');
  const libraryBoxes = libraries.map((lib) => checkbox(lib.name, value.library_ids?.includes(lib.id) ?? true, { value: lib.id }));
  const libraryGroup = h('fieldset.field.plain-fieldset',
    h('legend.label', 'Libraries'),
    libraries.length ? h('div.list.check-list', libraryBoxes) : h('p.hint', 'No libraries yet'));
  const adminNote = h('p.hint', 'Admins see everything.');

  const permissions = PERMISSIONS.filter(([key]) => key !== 'can_request' || acquisition)
    .map(([key, label, sub]) => [key, checkbox(label, Boolean(value.permissions?.[key]), {})]);
  const bitrate = select(BITRATES, value.permissions?.max_bitrate_kbps ?? '');
  const permissionGroup = h('fieldset.field.plain-fieldset',
    h('legend.label', 'Allowed'),
    h('div.list.check-list', permissions.map(([, el]) => el)));

  const sync = () => {
    libraryGroup.hidden = role === 'admin';
    adminNote.hidden = role !== 'admin';
  };
  sync();

  const el = h('div.form',
    h('div.field', h('span.label', { id: `${name}-label` }, 'Role'),
      segmented(name, [['member', 'Member'], ['admin', 'Admin']], role, (v) => { role = v; sync(); }, 'Role'),
      adminNote),
    libraryGroup,
    permissionGroup,
    field('Max streaming quality', bitrate));

  return {
    el,
    read() {
      const perms = {};
      for (const [key, box] of permissions) perms[key] = box.querySelector('input').checked;
      if (bitrate.value) perms.max_bitrate_kbps = Number(bitrate.value);
      const libraryIds = role === 'admin'
        ? libraries.map((l) => l.id)
        : libraryBoxes.filter((b) => b.querySelector('input').checked).map((b) => b.querySelector('input').value);
      return { role, permissions: perms, library_ids: libraryIds };
    },
  };
}
