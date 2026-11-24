// People: everyone with an account, and per person their access, devices, app keys and web
// sessions. A lost phone is removed from its owner's page.

import { call, describe } from '../api.js';
import {
  h, pageHead, section, list, row, badge, button, busy, field, input, toggle, agoInline, date, plural, num, bytes,
  confirm, toast, failed, shareLink, formError, setFieldError, checkbox, empty, platformIcon, platformName, icon,
} from '../ui.js';
import { accessForm } from './access.js';
import { approveForm } from './account.js';
import { roleName } from './common.js';

export async function render(ctx) {
  const [id] = ctx.args;
  if (id === 'new') return newPerson(ctx);
  if (id) return personPage(ctx, id);
  return peopleList(ctx);
}

const byRecent = (a, b) => Date.parse(b.last_seen_at || b.last_used_at || 0) - Date.parse(a.last_seen_at || a.last_used_at || 0);
const nameOf = (u) => u?.display_name || u?.username || 'Someone';

async function peopleList(ctx) {
  const [{ users }, { devices }] = await Promise.all([
    call('GET /admin/users'),
    call('GET /admin/devices').catch(() => ({ devices: [] })),
  ]);
  const count = new Map();
  for (const d of devices) count.set(d.user?.id, (count.get(d.user?.id) || 0) + 1);
  const sorted = [...users].sort((a, b) => (a.disabled - b.disabled) || ((a.role === 'admin' ? 0 : 1) - (b.role === 'admin' ? 0 : 1)) || nameOf(a).localeCompare(nameOf(b)));
  return h('div.page',
    pageHead('People', {
      actions: [
        h('a.btn', { href: '#/people/new' }, 'Add a person'),
        h('a.btn.primary', { href: '#/invites/new' }, 'Invite someone'),
      ],
    }),
    list(sorted.map((u) => {
      const n = count.get(u.id) || 0;
      return row({
        icon: 'account', tint: 'muted',
        title: [nameOf(u), u.role === 'admin' ? badge('Admin', 'accent') : null, u.disabled ? badge('Cannot sign in') : null],
        sub: `${plural(n, 'device')}, ${u.last_seen_at ? `seen ${agoInline(u.last_seen_at)}` : 'never signed in'}`,
        href: `#/people/${encodeURIComponent(u.id)}`,
        label: `${nameOf(u)}, ${roleName(u.role)}, ${plural(n, 'device')}`,
      });
    }), { iconInset: true }),
    section('Approve a code from Tracks', h('div.panel', approveForm('POST /admin/pairing/approve', 'Code shown in Tracks', 'The device joins your account.'))));
}

/* ---------- Add ---------- */

async function newPerson(ctx) {
  const { libraries } = await call('GET /admin/libraries');
  const display = input({ autocomplete: 'off', required: true });
  const username = input({ autocomplete: 'off', autocapitalize: 'none', spellcheck: 'false', required: true });
  const password = input({ type: 'password', autocomplete: 'new-password', required: true });
  const access = accessForm({ role: 'member', permissions: { can_download: true }, library_ids: libraries.map((l) => l.id) }, libraries, ctx);
  const error = formError();
  const submit = button('Add person', { kind: 'primary', type: 'submit' });

  const form = h('form.form', {
    novalidate: true,
    on: {
      submit: async (event) => {
        event.preventDefault();
        error.hide();
        [display, username, password].forEach((c) => setFieldError(c, ''));
        if (!username.value.trim()) { setFieldError(username, 'Choose a username.'); username.focus(); return; }
        if (!password.value) { setFieldError(password, 'Choose a password.'); password.focus(); return; }
        await busy(submit, async () => {
          try {
            const created = await call('POST /admin/users', { body: {
              username: username.value.trim(),
              display_name: display.value.trim() || username.value.trim(),
              password: password.value,
              ...access.read(),
            } });
            toast(`${nameOf(created)} added`);
            ctx.go(`people/${created.id}`);
          } catch (err) {
            if (err.code === 'conflict') { setFieldError(username, 'That username is taken.'); username.focus(); return; }
            error.show(describe(err));
          }
        });
      },
    },
  },
  h('div.panel.form',
    h('div.fields-2', field('Name', display), field('Username', username, { hint: 'What they type to sign in' })),
    field('Password', password, { hint: 'They can change it from their account page' })),
  h('div.panel', access.el),
  error.el,
  h('div.btn-row.end', h('a.btn', { href: '#/people' }, 'Cancel'), submit));

  return h('div.page.narrow', pageHead('Add a person', { crumb: ['People', '#/people'] }), form);
}

/* ---------- One person ---------- */

async function personPage(ctx, id) {
  const [user, { libraries }, { devices }, { keys }, { sessions }] = await Promise.all([
    call('GET /admin/users/{id}', { values: { id } }),
    call('GET /admin/libraries'),
    call('GET /admin/devices').catch(() => ({ devices: [] })),
    call('GET /admin/api-keys').catch(() => ({ keys: [] })),
    call('GET /admin/sessions').catch(() => ({ sessions: [] })),
  ]);
  const name = nameOf(user);
  const mine = (x) => x.user?.id === user.id;
  const theirDevices = devices.filter(mine).sort(byRecent);
  const theirKeys = keys.filter(mine).sort(byRecent);
  const theirSessions = sessions.filter(mine).sort(byRecent);
  const isSelf = user.id === ctx.user.id;

  const lede = [`${user.username}, ${roleName(user.role).toLowerCase()}`, user.last_seen_at ? `seen ${agoInline(user.last_seen_at)}` : 'never signed in', `joined ${date(user.created_at)}`].join(', ');
  // Counts only: what someone keeps in sync is theirs, so the content is never shown here.
  const synced = user.cloud ? h('p.small.muted', `Synced data: ${num(user.cloud.items ?? 0)} items · ${bytes(user.cloud.bytes ?? 0)}`) : null;
  return h('div.page.narrow',
    pageHead(name, { crumb: ['People', '#/people'], lede }),
    synced,
    user.disabled ? h('div.callout', { role: 'status' }, icon('lock', 'tint-warn'), h('p', `${name} cannot sign in. Their devices stop working until you turn sign-in back on below.`)) : null,
    devicesSection(ctx, name, theirDevices),
    keysSection(ctx, theirKeys),
    sessionsSection(ctx, user, name, theirSessions),
    accessSection(ctx, user, libraries),
    signInSection(ctx, user, name, isSelf),
    isSelf ? null : deleteSection(ctx, user, name));
}

/** A quiet Remove/Revoke/End button that asks first; the dialog carries the red. */
function revoker(ctx, { label, ariaLabel, title, text, route, id, done }) {
  const btn = button(label, { kind: 'danger' });
  btn.setAttribute('aria-label', ariaLabel);
  btn.addEventListener('click', async () => {
    if (!await confirm({ title, text, action: label })) return;
    await busy(btn, async () => {
      try {
        await call(route, { values: { id } });
        toast(done);
        ctx.rerender();
      } catch (err) { failed(err); }
    });
  });
  return btn;
}

function devicesSection(ctx, name, devices) {
  const rows = devices.map((d) => {
    const remove = revoker(ctx, {
      label: 'Remove', ariaLabel: `Remove ${d.name}`,
      title: `Remove ${d.name}?`,
      text: `${name} is signed out on it straight away. To use it again they connect it from their account.`,
      route: 'DELETE /admin/devices/{id}', id: d.id, done: `${d.name} removed`,
    });
    const what = [platformName(d.platform), d.app_name && d.app_version ? `${d.app_name} ${d.app_version}` : d.app_name].filter(Boolean).join(', ');
    return row({
      icon: platformIcon(d.platform), tint: 'muted',
      title: d.name,
      sub: `${what}, used ${agoInline(d.last_seen_at)}${d.last_ip ? ` from ${d.last_ip}` : ''}`,
      trail: remove,
    });
  });
  return section('Devices', list(rows.length ? rows : [empty('No devices yet', `${name} connects a phone from My account in the dashboard, or by typing a code from Tracks.`)], { iconInset: true }),
    { aside: rows.length ? h('span.num', plural(rows.length, 'device')) : null });
}

function keysSection(ctx, keys) {
  const rows = keys.map((k) => row({
    icon: 'key', tint: 'muted',
    title: [k.name, badge(k.kind === 'app_password' ? 'App password' : 'API key')],
    sub: k.last_used_at ? `Used ${agoInline(k.last_used_at)}${k.last_used_ip ? ` from ${k.last_used_ip}` : ''}` : 'Never used',
    trail: revoker(ctx, {
      label: 'Revoke', ariaLabel: `Revoke ${k.name}`,
      title: `Revoke ${k.name}?`, text: 'Any app using it is signed out straight away.',
      route: 'DELETE /admin/api-keys/{id}', id: k.id, done: `${k.name} revoked`,
    }),
  }));
  return section('App keys', list(rows.length ? rows : [empty('No app keys')], { iconInset: true }));
}

function sessionsSection(ctx, user, name, sessions) {
  const rows = sessions.map((s) => row({
    icon: 'account', tint: 'muted',
    title: [s.user_agent || 'A browser', s.current ? badge('This browser') : null],
    sub: `Active ${agoInline(s.last_seen_at)}${s.ip ? ` from ${s.ip}` : ''}`,
    trail: s.current ? null : revoker(ctx, {
      label: 'Sign out', ariaLabel: `Sign out ${s.user_agent || 'this browser'}`,
      title: 'Sign out this browser?', text: `${name} has to sign in again there.`,
      route: 'DELETE /admin/sessions/{id}', id: s.id, done: 'Signed out',
    }),
  }));

  const everywhere = button('Sign out everywhere', { kind: 'danger', iconName: 'signout' });
  everywhere.addEventListener('click', async () => {
    const also = checkbox('Also remove their devices', false, { sub: 'Their phones stop playing until they connect them again.' });
    const ok = await confirm({ title: `Sign ${name} out everywhere?`, text: 'Ends every web session they have.', extra: also, action: 'Sign out everywhere' });
    if (!ok) return;
    await busy(everywhere, async () => {
      try {
        await call('POST /admin/users/{id}/sign-out', { values: { id: user.id }, body: { devices: also.querySelector('input').checked } });
        toast(`${name} is signed out`);
      } catch (err) { failed(err); }
    });
    ctx.rerender();
  });

  return section('Signed in on the web', list(rows.length ? rows : [empty('Not signed in anywhere')], { iconInset: true }),
    { aside: everywhere });
}

function accessSection(ctx, user, libraries) {
  const display = input({ value: user.display_name || '', autocomplete: 'off' });
  const access = accessForm(user, libraries, ctx);
  const error = formError();
  const save = button('Save changes', { kind: 'primary', type: 'submit' });
  const form = h('form.form', {
    on: {
      submit: async (event) => {
        event.preventDefault();
        error.hide();
        await busy(save, async () => {
          const next = access.read();
          try {
            await call('PATCH /admin/users/{id}', { values: { id: user.id }, body: { display_name: display.value.trim() || user.username, role: next.role, permissions: { ...next.permissions, max_bitrate_kbps: next.permissions.max_bitrate_kbps ?? null } } });
            const before = [...(user.library_ids || [])].sort().join();
            if (next.role !== 'admin' && before !== [...next.library_ids].sort().join()) {
              await call('PUT /admin/users/{id}/libraries', { values: { id: user.id }, body: { library_ids: next.library_ids } });
            }
            toast('Saved');
            ctx.rerender();
          } catch (err) {
            error.show(describe(err));
          }
        });
      },
    },
  },
  h('div.panel.form', field('Name', display), access.el),
  error.el,
  h('div.btn-row.end', save));
  return section('Role and access', form);
}

function signInSection(ctx, user, name, isSelf) {
  const enabled = toggle(!user.disabled, { 'aria-label': 'Can sign in', disabled: isSelf });
  enabled.addEventListener('change', async () => {
    const on = enabled.checked;
    try {
      await call('PATCH /admin/users/{id}', { values: { id: user.id }, body: { disabled: !on } });
      toast(on ? `${name} can sign in` : `${name} can no longer sign in`);
      ctx.rerender();
    } catch (err) {
      enabled.checked = !on;
      failed(err);
    }
  });

  const resetSlot = h('div.detail', { hidden: true });
  const resetButton = button('Make a reset link', { iconName: 'link' });
  resetButton.addEventListener('click', () => busy(resetButton, async () => {
    try {
      const reset = await call('POST /admin/users/{id}/password-reset', { values: { id: user.id } });
      resetSlot.hidden = false;
      resetSlot.replaceChildren(h('div.form.tight.reveal',
        shareLink(reset.url, { label: 'Password reset link', title: 'Choose a new password', text: `${name}, use this link to choose a new password for Trackside.` }),
        h('p.secret-note', `Send it to ${name}. It works once, until ${date(reset.expires_at)}, and is not shown again.`)));
      resetSlot.querySelector('.btn')?.focus();
    } catch (err) { failed(err); }
  }));

  return section('Sign-in', h('div.list',
    h('div.setting-row',
      h('div.row-main', h('div.row-title', 'Can sign in'), h('div.row-sub', isSelf ? 'You cannot turn this off for yourself.' : 'Turn off to lock them out without deleting anything.')),
      h('div.control', enabled)),
    h('div.setting-row',
      h('div.row-main', h('div.row-title', 'Forgotten password'), h('div.row-sub', 'A link they open to choose a new one')),
      h('div.control', resetButton)),
    resetSlot));
}

function deleteSection(ctx, user, name) {
  const remove = button(`Delete ${name}`, { kind: 'danger', iconName: 'trash' });
  remove.addEventListener('click', async () => {
    const ok = await confirm({ title: `Delete ${name}?`, text: 'Their account, listening history, playlists, favourites and devices are deleted. Music files stay.', action: 'Delete' });
    if (!ok) return;
    await busy(remove, async () => {
      try {
        await call('DELETE /admin/users/{id}', { values: { id: user.id } });
        toast(`${name} deleted`);
        ctx.go('people');
      } catch (err) {
        failed(err.code === 'conflict' ? Object.assign(err, { details: { reason: 'last_admin' } }) : err);
      }
    });
  });
  return h('div.btn-row', remove);
}
