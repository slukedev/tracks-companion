import { call, describe, serverUrl } from '../api.js';
import { h, pageHead, section, list, row, badge, button, busy, field, input, select, empty, date, confirm, toast, failed, secret, formError } from '../ui.js';
import { accessForm } from './access.js';
import { roleName } from './common.js';

const STATE = { open: ['Open', 'good'], used: ['Used', ''], expired: ['Expired', ''], revoked: ['Revoked', ''] };

export async function render(ctx) {
  if (ctx.args[0] === 'new') return newInvite(ctx);
  const { invites } = await call('GET /admin/invites');
  const open = invites.filter((i) => i.state === 'open');
  const past = invites.filter((i) => i.state !== 'open');

  const inviteRow = (invite) => {
    const [label, kind] = STATE[invite.state] || [invite.state, ''];
    let trail = badge(label, kind);
    if (invite.state === 'open') {
      const revoke = button('Revoke', { kind: 'danger' });
      revoke.addEventListener('click', async () => {
        if (!await confirm({ title: 'Revoke this invite?', text: 'The link stops working now.', action: 'Revoke' })) return;
        await busy(revoke, async () => {
          try {
            await call('DELETE /admin/invites/{id}', { values: { id: invite.id } });
            toast('Invite revoked');
            ctx.rerender();
          } catch (err) { failed(err); }
        });
      });
      // The link and QR exist only in the response that created them, so an open invite
      // offers a replacement instead of trying to show them again.
      const renew = button('New Link', {});
      renew.setAttribute('aria-label', `New link for ${invite.note || 'this invite'}`);
      renew.addEventListener('click', async () => {
        if (!await confirm({ title: 'Make a new link?', text: 'The old link stops working.', action: 'New Link', danger: false })) return;
        await busy(renew, async () => {
          try {
            const days = Math.max(1, Math.round((Date.parse(invite.expires_at) - Date.parse(invite.created_at)) / 86400000));
            const created = await call('POST /admin/invites', { body: { role: invite.role, permissions: invite.permissions, library_ids: invite.library_ids, note: invite.note || '', expires_in_days: days } });
            await call('DELETE /admin/invites/{id}', { values: { id: invite.id } });
            const panel = result(created);
            panel.querySelector('.btn-row.end a').addEventListener('click', () => ctx.rerender());
            page.replaceWith(h('div.page.narrow', pageHead('New Link'), panel));
          } catch (err) { failed(err); }
        });
      });
      trail = [renew, revoke];
    }
    const when = invite.state === 'open' ? `until ${date(invite.expires_at)}`
      : invite.state === 'used' && invite.used_by ? invite.used_by : date(invite.created_at);
    return row({
      icon: 'invite', tint: invite.state === 'open' ? 'pink' : 'muted',
      title: invite.note || `${roleName(invite.role)} invite`,
      sub: `${roleName(invite.role)}, ${when}`,
      trail,
      wrapTrail: true,
    });
  };

  const page = h('div.page',
    pageHead('Invites', { actions: h('a.btn.primary', { href: '#/invites/new' }, 'New Invite') }),
    section('Open', list(open.length ? open.map(inviteRow) : [empty('No open invites', null, h('a.btn.primary', { href: '#/invites/new' }, 'New Invite'))], { iconInset: true })),
    past.length ? section('Earlier', list(past.map(inviteRow), { iconInset: true })) : null);
  return page;
}

async function newInvite(ctx) {
  const { libraries } = await call('GET /admin/libraries');
  const note = input({ autocomplete: 'off', maxlength: '120' });
  const days = select([['1', '1 day'], ['3', '3 days'], ['7', '7 days'], ['14', '14 days'], ['30', '30 days']], '7');
  const access = accessForm({ role: 'member', permissions: { can_download: true }, library_ids: libraries.map((l) => l.id) }, libraries, ctx);
  const error = formError();
  const submit = button('Create Invite', { kind: 'primary', type: 'submit' });
  const page = h('div.page.narrow', pageHead('New Invite'));

  const form = h('form.form', {
    on: {
      submit: async (event) => {
        event.preventDefault();
        error.hide();
        await busy(submit, async () => {
          try {
            const created = await call('POST /admin/invites', { body: { note: note.value.trim(), expires_in_days: Number(days.value), ...access.read() } });
            form.replaceWith(result(created));
          } catch (err) { error.show(describe(err)); }
        });
      },
    },
  },
  h('div.panel.form',
    h('div.fields-2',
      field('Note', note),
      field('Expires', days))),
  h('div.panel', access.el),
  error.el,
  h('div.btn-row.end', h('a.btn', { href: '#/invites' }, 'Cancel'), submit));

  page.append(form);
  return page;
}

function result(invite) {
  const panel = h('div.panel.reveal',
    h('h2', 'Invite Ready'),
    h('div.pairing',
      invite.qr_svg_url ? h('div.qr', h('img', { src: serverUrl(invite.qr_svg_url), alt: 'QR code for the invite link', width: '176', height: '176' })) : null,
      h('div.form',
        secret(invite.url, 'Invite link'),
        h('p.small.muted', `Shown once. Until ${date(invite.expires_at)}.`))),
    h('div.btn-row.end', h('a.btn.primary', { href: '#/invites' }, 'Done')));
  setTimeout(() => panel.querySelector('input')?.focus(), 0);
  return panel;
}
