import { call } from '../api.js';
import { h, pageHead, section, list, row, badge, button, busy, empty, ago, agoInline, confirm, toast, failed } from '../ui.js';
import { approveForm } from './account.js';

export async function render(ctx) {
  const [{ devices }, { keys }, { sessions }] = await Promise.all([
    call('GET /admin/devices'),
    call('GET /admin/api-keys'),
    call('GET /admin/sessions'),
  ]);
  const who = (u) => u?.display_name || u?.username || 'Unknown';

  const revoker = (label, title, text, route, id) => {
    const btn = button(label, { kind: 'danger' });
    btn.addEventListener('click', async () => {
      if (!await confirm({ title, text, action: label })) return;
      await busy(btn, async () => {
        try {
          await call(route, { values: { id } });
          toast('Done');
          ctx.rerender();
        } catch (err) { failed(err); }
      });
    });
    return btn;
  };

  const byRecent = (a, b) => Date.parse(b.last_seen_at || b.last_used_at || 0) - Date.parse(a.last_seen_at || a.last_used_at || 0);

  const deviceRows = [...devices].sort(byRecent).map((d) => {
    const btn = revoker('Remove', `Remove ${d.name}?`, `${who(d.user)} is signed out on it now.`, 'DELETE /admin/devices/{id}', d.id);
    btn.setAttribute('aria-label', `Remove ${d.name}`);
    return row({
      icon: 'devices', tint: 'indigo',
      title: d.name,
      sub: `${who(d.user)}, ${agoInline(d.last_seen_at)}`,
      trail: btn,
    });
  });

  const keyRows = [...keys].sort(byRecent).map((k) => {
    const btn = revoker('Revoke', `Revoke ${k.name}?`, `Apps using it are signed out now.`, 'DELETE /admin/api-keys/{id}', k.id);
    btn.setAttribute('aria-label', `Revoke ${k.name}`);
    return row({
      icon: 'key', tint: 'indigo',
      title: [k.name, badge(k.kind === 'app_password' ? 'App password' : 'Key')],
      sub: `${who(k.user)}, ${k.last_used_at ? agoInline(k.last_used_at) : 'never used'}`,
      trail: btn,
    });
  });

  const sessionRows = [...sessions].sort(byRecent).map((s) => row({
    icon: 'account', tint: 'muted',
    title: [who(s.user), s.current ? badge('You') : null],
    sub: `${s.user_agent || 'Browser'}, ${agoInline(s.last_seen_at)}`,
    trail: s.current ? null : (() => {
      const btn = revoker('End', 'End this session?', `${who(s.user)} signs in again in that browser.`, 'DELETE /admin/sessions/{id}', s.id);
      btn.setAttribute('aria-label', `End session for ${who(s.user)}`);
      return btn;
    })(),
  }));

  return h('div.page',
    pageHead('Devices and Keys'),
    section('Approve a Code', h('div.panel', approveForm('POST /admin/pairing/approve', 'Code from Tracks'))),
    section('Devices', list(deviceRows.length ? deviceRows : [empty('No devices')], { iconInset: true })),
    section('App Keys', list(keyRows.length ? keyRows : [empty('No keys')], { iconInset: true })),
    section('Web Sessions', list(sessionRows, { iconInset: true })));
}
