import { call, href, ensureRecentAuth } from '../api.js';
import { h, pageHead, section, list, row, badge, button, busy, empty, ago, full, when, bytes, confirm, toast, failed, codeLine, capital } from '../ui.js';

const KIND = { scheduled: 'Scheduled', manual: 'Made by you', pre_migrate: 'Before an update' };

/** "daily 03:30" becomes "Every day at 03:30"; anything else is shown as written. */
export function scheduleText(schedule) {
  const m = /^daily (\d\d:\d\d)$/.exec(schedule || '');
  if (m) return `Every day at ${m[1]}`;
  return capital(schedule);
}

export async function render(ctx) {
  const [{ backups, next_at: nextAt }, settings] = await Promise.all([
    call('GET /admin/backups'),
    call('GET /admin/settings').catch(() => ({})),
  ]);
  const schedule = settings['backup.schedule']?.value;
  const keep = settings['backup.keep']?.value;

  const now = button('Back up now', { kind: 'primary', iconName: 'backups' });
  now.addEventListener('click', () => busy(now, async () => {
    try {
      await call('POST /admin/backups');
      toast('Backup started');
      setTimeout(() => { if (ctx.isCurrent()) ctx.rerender(); }, 3000);
    } catch (err) {
      failed(err.code === 'conflict' ? Object.assign(err, { details: { reason: 'running' } }) : err);
    }
  }));

  const rows = [...backups].sort((a, b) => Date.parse(b.at) - Date.parse(a.at)).map((b) => {
    const del = button('Delete', { kind: 'danger' });
    del.setAttribute('aria-label', `Delete the backup from ${full(b.at)}`);
    del.addEventListener('click', async () => {
      if (!await confirm({ title: 'Delete this backup?', text: `The backup from ${full(b.at)} (${bytes(b.bytes)}) is deleted for good.`, action: 'Delete backup' })) return;
      await busy(del, async () => {
        try {
          await call('DELETE /admin/backups/{name}', { values: { name: b.name } });
          toast('Backup deleted');
          ctx.rerender();
        } catch (err) { failed(err); }
      });
    });
    return row({
      icon: 'backups', tint: 'muted',
      title: [capital(ago(b.at)), b.kind !== 'scheduled' ? badge(KIND[b.kind] || b.kind) : null],
      sub: bytes(b.bytes),
      trail: [downloadButton(b), del],
      wrapTrail: true,
    });
  });

  const on = schedule && schedule !== 'off';
  const lede = on
    ? `${scheduleText(schedule)}, keeping the last ${keep ?? 7}.${nextAt ? ` Next one ${when(nextAt)}.` : ''}`
    : 'Automatic backups are off.';
  return h('div.page.narrow',
    pageHead('Backups', { lede, actions: now }),
    section('Saved backups', list(rows.length ? rows : [empty('No backups yet', 'A backup holds accounts, settings, playlists and listening history. Music files are not included.')], { iconInset: true }),
      { aside: h('a', { href: '#/settings' }, 'Change schedule') }),
    section('Restore', h('div.panel',
      h('p', 'Restoring replaces everything on the server with the backup. Stop Trackside, then run this on the host with the backup file:'),
      codeLine('trackside backup restore <file>', 'Copy command'))));
}

/** Backups hold everything on the server, so the download asks for a recent sign-in first. */
function downloadButton(b) {
  const link = h('a.btn', { href: href('GET /admin/backups/{name}', { name: b.name }), download: b.name, 'aria-label': `Download the backup from ${full(b.at)}` }, 'Download');
  link.addEventListener('click', async (event) => {
    if (link.dataset.confirmed) { delete link.dataset.confirmed; return; }
    event.preventDefault();
    try {
      if (await ensureRecentAuth()) { link.dataset.confirmed = '1'; link.click(); }
    } catch (err) { failed(err); }
  });
  return link;
}
