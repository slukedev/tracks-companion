import { call, describe } from '../api.js';
import {
  h, pageHead, section, list, row, badge, nextId, button, busy, field, input, select, toggle, checkbox, empty,
  ago, agoInline, num, plural, confirm, toast, failed, formError, setFieldError, icon, copyButton,
} from '../ui.js';
import { libraryBadge, songs, scanKind, scanSummary } from './common.js';

const INTERVALS = [['0', 'Off'], ['900', 'Every 15 minutes'], ['1800', 'Every 30 minutes'], ['3600', 'Every hour'], ['21600', 'Every 6 hours'], ['86400', 'Every day']];

export async function render(ctx) {
  const [id] = ctx.args;
  if (id === 'add') return addLibrary(ctx);
  if (id) return libraryDetail(ctx, id);
  return libraryList(ctx);
}

async function libraryList(ctx) {
  const [{ libraries }, imports, duplicates] = await Promise.all([
    call('GET /admin/libraries'),
    call('GET /admin/imports', { query: { limit: 20 } }).catch(() => ({ imports: [] })),
    call('GET /admin/duplicates', { query: { limit: 20 } }).catch(() => ({ groups: [] })),
  ]);
  const head = pageHead('Libraries', { actions: h('a.btn.primary', { href: '#/libraries/add' }, 'Add Library') });
  if (!libraries.length) {
    return h('div.page', head, list(empty('No music yet', null, h('a.btn.primary', { href: '#/libraries/add' }, 'Add Library'))));
  }
  return h('div.page',
    head,
    section(plural(libraries.length, 'library', 'libraries'), list(libraries.map((lib) => row({
      icon: lib.state === 'unavailable' ? 'warning' : 'library',
      tint: lib.state === 'unavailable' ? 'warn' : 'blue',
      title: [lib.name, libraryBadge(lib)],
      sub: [h('span.mono', lib.root), `, ${songs(lib.song_count)}`],
      href: `#/libraries/${encodeURIComponent(lib.id)}`,
    })), { iconInset: true })),
    targetsSection(ctx, libraries),
    imports.imports.length ? importsSection(imports.imports) : null,
    duplicates.groups.length ? duplicatesSection(duplicates.groups, libraries) : null);
}

function targetsSection(ctx, libraries) {
  const writable = libraries.filter((l) => l.writable);
  const current = (flag) => libraries.find((l) => l[flag])?.id || '';
  const options = [['', 'None'], ...writable.map((l) => [l.id, l.name])];
  const uploads = select(options, current('uploads_target'));
  const acquire = ctx.acquisition ? select(options, current('acquire_target')) : null;
  const save = async () => {
    const body = { uploads_library: uploads.value || undefined };
    if (acquire) body.acquire_library = acquire.value || undefined;
    try {
      await call('PUT /admin/settings/targets', { body });
      toast('Saved');
    } catch (err) { failed(err); }
  };
  uploads.addEventListener('change', save);
  acquire?.addEventListener('change', save);
  const rowFor = (title, sub, control) => h('div.setting-row', h('div.row-main', h('label.row-title', { for: (control.id = nextId('t')) }, title), h('div.row-sub', sub)), h('div.control', control));
  return section('Where New Songs Go', h('div.list',
    rowFor('Uploads', null, uploads),
    acquire ? rowFor('Requests', null, acquire) : null),
  { foot: writable.length ? null : 'Make a library writable first.' });
}

const IMPORT_STATE = { receiving: ['Uploading', ''], staged: ['Checking', ''], imported: ['Added', 'good'], duplicate: ['Already there', ''], rejected: ['Rejected', 'warn'], failed: ['Failed', 'bad'] };

function importsSection(imports) {
  return section('Recent Uploads', list(imports.map((i) => {
    const [label, kind] = IMPORT_STATE[i.state] || [i.state, ''];
    return row({
      icon: 'download', tint: 'muted',
      title: i.file_name,
      sub: [[i.source === 'inbox' ? 'Inbox folder' : i.user?.display_name || 'Upload', agoInline(i.created_at)].join(', '), i.error ? h('div', i.error) : null],
      trail: badge(label, kind),
    });
  }), { iconInset: true }));
}

function duplicatesSection(groups, libraries) {
  const libName = (id) => libraries.find((l) => l.id === id)?.name || 'Unknown library';
  return section('Duplicates', list(groups.map((g) => h('div.row',
    h('div.row-main',
      h('div.row-title', `${g.songs[0].title}, ${g.songs[0].artist}`),
      g.songs.map((s) => h('div.row-sub', h('span', `${libName(s.library_id)}: `), h('span.mono', s.path))))))),
  {});
}

/* ---------- Add ---------- */

async function addLibrary(ctx) {
  const name = input({ autocomplete: 'off', required: true, maxlength: '64' });
  const root = input({ autocomplete: 'off', spellcheck: 'false', required: true, mono: true, placeholder: '/music' });
  const writable = checkbox('Writable', false);
  const watch = checkbox('Watch for changes', true);
  const grant = checkbox('Everyone can see it', true);
  const error = formError();
  const submit = button('Add Library', { kind: 'primary', type: 'submit' });
  const form = h('form.form', {
    novalidate: true,
    on: {
      submit: async (event) => {
        event.preventDefault();
        error.hide();
        setFieldError(name, '');
        setFieldError(root, '');
        if (!name.value.trim()) { setFieldError(name, 'Name the library.'); name.focus(); return; }
        if (!root.value.trim().startsWith('/') && !/^[A-Za-z]:[\\/]/.test(root.value.trim())) { setFieldError(root, 'Use a full path, like /music.'); root.focus(); return; }
        await busy(submit, async () => {
          try {
            const created = await call('POST /admin/libraries', { body: {
              name: name.value.trim(),
              root: root.value.trim(),
              writable: writable.querySelector('input').checked,
              watch: watch.querySelector('input').checked,
              grant_all_users: grant.querySelector('input').checked,
            } });
            toast('Library added');
            ctx.go(`libraries/${created.id}`);
          } catch (err) {
            if (err.code === 'bad_request' && err.details?.field === 'root') { setFieldError(root, err.message || 'That folder cannot be used.'); root.focus(); return; }
            error.show(describe(err));
          }
        });
      },
    },
  },
  h('div.panel.form',
    field('Name', name),
    field('Folder', root, { hint: 'Path on the server' })),
  h('div.panel.form', writable, watch, grant),
  error.el,
  h('div.btn-row.end', h('a.btn', { href: '#/libraries' }, 'Cancel'), submit));
  return h('div.page.narrow', pageHead('Add Library'), form);
}

/* ---------- Detail ---------- */

async function libraryDetail(ctx, id) {
  const [{ libraries }, { scans }, problems] = await Promise.all([
    call('GET /admin/libraries'),
    call('GET /admin/libraries/{id}/scans', { values: { id }, query: { limit: 10 } }),
    call('GET /admin/libraries/{id}/problems', { values: { id } }).catch(() => null),
  ]);
  const lib = libraries.find((l) => l.id === id);
  if (!lib) {
    return h('div.page', pageHead('Library not found'), list(empty('This library no longer exists', null, h('a.btn', { href: '#/libraries' }, 'All Libraries'))));
  }
  const running = scans.find((s) => s.state === 'running');
  const lastAborted = scans[0]?.state === 'aborted';

  const scanButton = (kind, label, primary) => {
    const btn = button(label, { kind: primary ? 'primary' : '', iconName: primary ? 'scan' : undefined, disabled: Boolean(running) || lib.state === 'unavailable' });
    btn.addEventListener('click', () => busy(btn, () => startScan(ctx, lib, kind, false)));
    return btn;
  };

  const page = h('div.page.narrow',
    h('header.page-head',
      h('div',
        h('p.small', h('a', { href: '#/libraries' }, 'Libraries')),
        h('h1', { tabindex: '-1', 'data-page-title': '' }, lib.name),
        h('p.lede', h('span.mono', lib.root))),
      h('div.actions', scanButton('full', 'Full Scan', false), scanButton('quick', 'Scan Now', true))),
    lib.state === 'unavailable' ? h('div.callout', { role: 'status' }, icon('warning', 'tint-warn'),
      h('div.form',
        h('p', h('strong', 'Offline'), lib.unavailable_since ? ` since ${agoInline(lib.unavailable_since)}. ` : '. ', lib.unavailable_reason || ''),
        h('p.muted', 'Songs are kept until it is back.'))) : null,
    lib.watching === 'polling' && lib.watch ? watchLimit() : null,
    lastAborted && lib.state !== 'unavailable' ? forceScan(ctx, lib, scans[0]) : null,
    scansSection(ctx, lib, scans),
    problems ? problemsSection(problems) : null,
    settingsSection(ctx, lib),
    removeSection(ctx, lib));

  if (running) {
    const timer = setInterval(async () => {
      if (!ctx.isCurrent()) { clearInterval(timer); return; }
      const latest = await call('GET /admin/libraries/{id}/scans', { values: { id }, query: { limit: 1 } }).catch(() => null);
      const run = latest?.scans?.[0];
      if (!run) return;
      if (run.state !== 'running') { clearInterval(timer); ctx.rerender(); return; }
      const live = page.querySelector('[data-live-scan]');
      if (live) live.textContent = `${num(run.files_seen)} files checked, ${num(run.folders_seen)} folders`;
    }, 2000);
    ctx.onLeave(() => clearInterval(timer));
  }
  return page;
}

async function startScan(ctx, lib, kind, force) {
  try {
    await call('POST /admin/libraries/{id}/scan', { values: { id: lib.id }, body: { kind, force_empty: force } });
    toast(`${scanKind(kind)} started`);
    ctx.rerender();
  } catch (err) {
    failed(err);
  }
}

function forceScan(ctx, lib, run) {
  const btn = button('Scan Anyway', {});
  btn.addEventListener('click', async () => {
    const ok = await confirm({
      title: 'Scan an empty folder?',
      text: 'Every song in this library will be marked missing.',
      action: 'Scan Anyway',
    });
    if (ok) await busy(btn, () => startScan(ctx, lib, 'full', true));
  });
  return h('div.callout', icon('warning', 'tint-warn'),
    h('div.form', h('p', 'Last scan stopped: the folder looked empty.'), h('div.btn-row', btn)));
}

function scansSection(ctx, lib, scans) {
  if (!scans.length) return section('Scans', list(empty('No scans yet', null)));
  return section('Scans', list(scans.map((run) => {
    if (run.state === 'running') {
      return h('div.row',
        icon('scan', 'tint-accent'),
        h('div.row-main',
          h('div.row-title', `${scanKind(run.kind)} in progress`, h('span.dot.live', { 'aria-hidden': 'true' })),
          h('div.row-sub', { 'data-live-scan': '', 'aria-live': 'polite' }, `${num(run.files_seen)} files`)));
    }
    const errors = typeof run.errors === 'number' ? run.errors : run.errors?.length || 0;
    const tint = run.state === 'done' ? 'blue' : 'warn';
    const detail = h('div.detail', { hidden: true });
    const sub = ago(run.finished_at || run.started_at);
    if (!errors) {
      return row({ icon: 'scan', tint, title: [`${scanKind(run.kind)}: ${run.state === 'done' ? scanSummary(run) : run.state}`], sub: run.message ? `${sub}. ${run.message}` : sub });
    }
    const toggleBtn = button(`Show ${plural(errors, 'Error')}`, { kind: 'plain' });
    toggleBtn.setAttribute('aria-expanded', 'false');
    toggleBtn.addEventListener('click', async () => {
      const open = toggleBtn.getAttribute('aria-expanded') === 'true';
      toggleBtn.setAttribute('aria-expanded', String(!open));
      toggleBtn.lastChild.textContent = open ? `Show ${plural(errors, 'Error')}` : 'Hide Errors';
      detail.hidden = open;
      if (!open && !detail.childElementCount) {
        try {
          const full = await call('GET /admin/scans/{id}', { values: { id: run.id } });
          detail.append(...full.errors.map((e) => h('div.small', h('div.mono', e.path), h('div.muted', e.error))));
        } catch (err) { detail.append(h('p.small', describe(err))); }
      }
    });
    return h('div', row({ icon: 'scan', tint, title: `${scanKind(run.kind)}: ${scanSummary(run)}`, sub, trail: toggleBtn }), detail);
  }), { iconInset: true }));
}

function problemsSection(p) {
  const rows = [
    ...p.tag_problems.map((t) => row({ icon: 'info', tint: 'muted', title: h('span.mono', t.path), sub: t.problem })),
    ...p.quarantined.map((q) => row({ icon: 'warning', tint: 'warn', title: h('span.mono', q.path), sub: `Skipped: ${q.error}` })),
    ...p.unreadable_folders.map((f) => row({ icon: 'lock', tint: 'warn', title: h('span.mono', f.path), sub: 'Unreadable' })),
  ];
  if (!rows.length) return null;
  return section('Files with Problems', list(rows, { iconInset: true }), {});
}

/** File watching fell back to polling: the fix is one command on the host. */
function watchLimit() {
  const command = 'sudo sysctl -w fs.inotify.max_user_watches=524288';
  return h('div.callout', icon('warning', 'tint-warn'),
    h('div.form.tight',
      h('p', h('strong', 'Not watched live.'), ' New files show up on the next scan. Run on the host:'),
      h('div.secret', h('code.input.mono', command), copyButton(command))));
}

function settingsSection(ctx, lib) {
  const name = input({ value: lib.name, maxlength: '64', 'aria-label': 'Library name' });
  const enabled = toggle(lib.enabled, { 'aria-label': 'Enabled' });
  const watch = toggle(lib.watch, { 'aria-label': 'Watch for changes' });
  const writable = toggle(lib.writable, { 'aria-label': 'Writable' });
  const interval = select(INTERVALS.some(([v]) => Number(v) === lib.quick_scan_interval_s) ? INTERVALS : [...INTERVALS, [String(lib.quick_scan_interval_s), `Every ${lib.quick_scan_interval_s} seconds`]], String(lib.quick_scan_interval_s ?? 1800), { 'aria-label': 'Quick scan' });
  const patch = async (body, revert) => {
    try {
      await call('PATCH /admin/libraries/{id}', { values: { id: lib.id }, body });
      toast('Saved');
    } catch (err) { revert?.(); failed(err); }
  };
  name.addEventListener('change', () => { if (name.value.trim()) patch({ name: name.value.trim() }, () => { name.value = lib.name; }); });
  enabled.addEventListener('change', () => patch({ enabled: enabled.checked }, () => { enabled.checked = !enabled.checked; }));
  watch.addEventListener('change', () => patch({ watch: watch.checked }, () => { watch.checked = !watch.checked; }));
  writable.addEventListener('change', () => patch({ writable: writable.checked }, () => { writable.checked = !writable.checked; }));
  interval.addEventListener('change', () => patch({ quick_scan_interval_s: Number(interval.value) }));

  const setting = (title, sub, control) => h('div.setting-row', h('div.row-main', h('div.row-title', title), sub ? h('div.row-sub', sub) : null), h('div.control', control));

  return section('Settings', h('div.list',
    setting('Name', null, name),
    setting('Enabled', null, enabled),
    setting('Watch for changes', null, watch),
    setting('Quick scan', null, interval),
    setting('Writable', null, writable)));
}

function removeSection(ctx, lib) {
  const btn = button('Remove Library', { kind: 'danger', iconName: 'trash' });
  btn.addEventListener('click', async () => {
    const ok = await confirm({
      title: `Remove ${lib.name}?`,
      text: `Its ${songs(lib.song_count)} disappear from everyone's apps. Files on disk stay.`,
      action: 'Remove',
    });
    if (!ok) return;
    await busy(btn, async () => {
      try {
        await call('DELETE /admin/libraries/{id}', { values: { id: lib.id } });
        toast(`${lib.name} removed`);
        ctx.go('libraries');
      } catch (err) { failed(err); }
    });
  });
  return h('div.btn-row', btn);
}

