import { call, href, ensureRecentAuth } from '../api.js';
import { h, pageHead, section, list, row, badge, button, busy, empty, ago, bytes, confirm, toast, failed, copyButton } from '../ui.js';

const KIND = { scheduled: 'Scheduled', manual: 'Made by hand', pre_migrate: 'Before an update' };

export async function render(ctx) {
  const [{ backups }, settings] = await Promise.all([
    call('GET /admin/backups'),
    call('GET /admin/settings').catch(() => ({})),
  ]);
  const schedule = settings['backup.schedule']?.value;
  const keep = settings['backup.keep']?.value;

  const now = button('Back Up Now', { kind: 'primary', iconName: 'backups' });
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
    del.setAttribute('aria-label', `Delete ${b.name}`);
    del.addEventListener('click', async () => {
      if (!await confirm({ title: 'Delete this backup?', text: `${b.name} is deleted from the data folder.`, action: 'Delete' })) return;
      await busy(del, async () => {
        try {
          await call('DELETE /admin/backups/{name}', { values: { name: b.name } });
          toast('Backup deleted');
          ctx.rerender();
        } catch (err) { failed(err); }
      });
    });
    return row({
      icon: 'backups', tint: 'teal',
      title: [ago(b.at), b.kind !== 'scheduled' ? badge(KIND[b.kind] || b.kind) : null],
      sub: bytes(b.bytes),
      trail: [downloadButton(b), del],
      wrapTrail: true,
    });
  });

  const command = 'trackside backup restore <file>';
  return h('div.page.narrow',
    pageHead('Backups', {
      lede: schedule && schedule !== 'off' ? `${schedule[0].toUpperCase()}${schedule.slice(1)}, keep ${keep ?? 7}` : 'Automatic backups off',
      actions: now,
    }),
    section('Backups', list(rows.length ? rows : [empty('No backups yet')], { iconInset: true }),
      { aside: h('a', { href: '#/settings' }, 'Schedule') }),
    section('Restore', h('div.panel',
      h('p', 'Stop the server, then run:'),
      h('div.secret', h('code.input.mono', command), copyButton(command)),
    )));
}

/** Backups hold everything on the server, so the download asks for a recent sign-in first. */
function downloadButton(b) {
  const link = h('a.btn', { href: href('GET /admin/backups/{name}', { name: b.name }), download: b.name, 'aria-label': `Download ${b.name}` }, 'Download');
  link.addEventListener('click', async (event) => {
    if (link.dataset.confirmed) { delete link.dataset.confirmed; return; }
    event.preventDefault();
    try {
      if (await ensureRecentAuth()) { link.dataset.confirmed = '1'; link.click(); }
    } catch (err) { failed(err); }
  });
  return link;
}
