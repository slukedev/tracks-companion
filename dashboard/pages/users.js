import { call, describe } from '../api.js';
import {
  h, pageHead, section, list, row, kv, badge, button, busy, field, input, toggle, ago,
  confirm, toast, failed, secret, formError, setFieldError, date, checkbox,
} from '../ui.js';
import { accessForm } from './access.js';
import { roleName } from './common.js';

export async function render(ctx) {
  const [id] = ctx.args;
  if (id === 'new') return newUser(ctx);
  if (id) return userDetail(ctx, id);
  return userList(ctx);
}

async function userList(ctx) {
  const [{ users }, { libraries }] = await Promise.all([call('GET /admin/users'), call('GET /admin/libraries')]);
  const sorted = [...users].sort((a, b) => (a.disabled - b.disabled) || (a.role === 'admin' ? -1 : 0) - (b.role === 'admin' ? -1 : 0) || a.display_name.localeCompare(b.display_name));
  return h('div.page',
    pageHead('Users', {
      actions: [
        h('a.btn', { href: '#/users/new' }, 'Add User'),
        h('a.btn.primary', { href: '#/invites/new' }, 'Invite Someone'),
      ],
    }),
    list(sorted.map((u) => row({
      title: [u.display_name || u.username, u.role === 'admin' ? badge('Admin', 'accent') : null, u.disabled ? badge('Disabled') : null],
      sub: u.username,
      trail: h('span.small.nowrap', u.last_seen_at ? ago(u.last_seen_at) : 'Never'),
      href: `#/users/${encodeURIComponent(u.id)}`,
      label: `${u.display_name || u.username}, ${roleName(u.role)}`,
    }))));
}

async function newUser(ctx) {
  const { libraries } = await call('GET /admin/libraries');
  const display = input({ autocomplete: 'off', required: true });
  const username = input({ autocomplete: 'off', autocapitalize: 'none', spellcheck: 'false', required: true });
  const password = input({ type: 'password', autocomplete: 'new-password', required: true });
  const access = accessForm({ role: 'member', permissions: { can_download: true }, library_ids: libraries.map((l) => l.id) }, libraries, ctx);
  const error = formError();
  const submit = button('Add User', { kind: 'primary', type: 'submit' });

  const form = h('form.form', {
    novalidate: true,
    on: {
      submit: async (event) => {
        event.preventDefault();
        error.hide();
        [display, username, password].forEach((c) => setFieldError(c, ''));
        if (!username.value.trim()) { setFieldError(username, 'Choose a username.'); username.focus(); return; }
        await busy(submit, async () => {
          try {
            const created = await call('POST /admin/users', { body: {
              username: username.value.trim(),
              display_name: display.value.trim() || username.value.trim(),
              password: password.value,
              ...access.read(),
            } });
            toast('User added');
            ctx.go(`users/${created.id}`);
          } catch (err) {
            if (err.code === 'conflict') { setFieldError(username, 'That username is taken.'); username.focus(); return; }
            error.show(describe(err));
          }
        });
      },
    },
  },
  h('div.panel.form',
    h('div.fields-2', field('Name', display), field('Username', username)),
    field('Password', password)),
  h('div.panel', access.el),
  error.el,
  h('div.btn-row.end', h('a.btn', { href: '#/users' }, 'Cancel'), submit));

  return h('div.page.narrow', pageHead('Add User'), form);
}

async function userDetail(ctx, id) {
  const [user, { libraries }] = await Promise.all([
    call('GET /admin/users/{id}', { values: { id } }),
    call('GET /admin/libraries'),
  ]);
  const name = user.display_name || user.username;
  const display = input({ value: user.display_name || '', autocomplete: 'off' });
  const access = accessForm(user, libraries, ctx);
  const error = formError();
  const save = button('Save Changes', { kind: 'primary', type: 'submit' });

  const form = h('form.form', {
    on: {
      submit: async (event) => {
        event.preventDefault();
        error.hide();
        await busy(save, async () => {
          const next = access.read();
          try {
            await call('PATCH /admin/users/{id}', { values: { id }, body: { display_name: display.value.trim() || user.username, role: next.role, permissions: next.permissions } });
            const before = [...(user.library_ids || [])].sort().join();
            if (next.role !== 'admin' && before !== [...next.library_ids].sort().join()) {
              await call('PUT /admin/users/{id}/libraries', { values: { id }, body: { library_ids: next.library_ids } });
            }
            toast('Saved');
            ctx.rerender();
          } catch (err) {
            error.show(err.details?.reason === 'last_admin' ? 'The only admin. Promote someone first.' : describe(err));
          }
        });
      },
    },
  },
  h('div.panel.form', field('Name', display), access.el),
  error.el,
  h('div.btn-row.end', save));

  const resetSlot = h('div.detail', { hidden: true });
  const resetButton = button('Make Link', { iconName: 'link' });
  resetButton.addEventListener('click', () => busy(resetButton, async () => {
    try {
      const reset = await call('POST /admin/users/{id}/password-reset', { values: { id } });
      resetSlot.hidden = false;
      resetSlot.replaceChildren(h('div.form.reveal',
        secret(reset.url, 'Password reset link'),
        h('p.secret-note', `Shown once. Works until ${date(reset.expires_at)}.`)));
    } catch (err) { failed(err); }
  }));

  const signOut = button('End All', { iconName: 'signout' });
  signOut.addEventListener('click', async () => {
    const also = checkbox('Also remove their devices', false);
    const ok = await confirm({ title: `End ${name}'s sessions?`, text: 'Ends their web sessions.', extra: also, action: 'End All', danger: false });
    if (!ok) return;
    await busy(signOut, async () => {
      try {
        await call('POST /admin/users/{id}/sign-out', { values: { id }, body: { devices: also.querySelector('input').checked } });
        toast('Signed out');
      } catch (err) { failed(err); }
    });
  });

  const disabled = toggle(!user.disabled, { 'aria-label': 'Account enabled' });
  disabled.addEventListener('change', async () => {
    const enable = disabled.checked;
    try {
      await call('PATCH /admin/users/{id}', { values: { id }, body: { disabled: !enable } });
      toast(enable ? 'Enabled' : 'Disabled');
    } catch (err) {
      disabled.checked = !enable;
      failed(err);
    }
  });

  const remove = button('Delete User', { kind: 'danger', iconName: 'trash' });
  remove.addEventListener('click', async () => {
    const ok = await confirm({ title: `Delete ${name}?`, text: 'Deletes their history, playlists and devices. Music files stay.', action: 'Delete User' });
    if (!ok) return;
    await busy(remove, async () => {
      try {
        await call('DELETE /admin/users/{id}', { values: { id } });
        toast(`${name} deleted`);
        ctx.go('users');
      } catch (err) {
        failed(err.code === 'conflict' ? Object.assign(err, { details: { reason: 'last_admin' } }) : err);
      }
    });
  });

  return h('div.page.narrow',
    h('header.page-head',
      h('div',
        h('p.small', h('a', { href: '#/users' }, 'Users')),
        h('h1', { tabindex: '-1', 'data-page-title': '' }, name),
        null)),
    h('dl.list',
      kv('Last seen', user.last_seen_at ? ago(user.last_seen_at) : 'Never'),
      kv('Devices', String(user.devices ?? 0))),
    section('Access', form),
    section('Sign-in', h('div.list',
      h('div.setting-row', h('div.row-main', h('div.row-title', 'Can sign in')), h('div.control', disabled)),
      h('div.setting-row', h('div.row-main', h('div.row-title', 'Password reset')), h('div.control', resetButton)),
      resetSlot,
      h('div.setting-row', h('div.row-main', h('div.row-title', 'Web sessions')), h('div.control', signOut)))),
    h('div.btn-row', remove));
}
