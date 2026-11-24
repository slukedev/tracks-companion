import { call, describe } from '../api.js';
import {
  h, pageHead, section, list, row, badge, nextId, button, busy, field, input, select, toggle, checkbox, empty,
  ago, agoInline, num, plural, confirm, toast, failed, formError, setFieldError, icon, codeLine, segmented,
} from '../ui.js';
import { libraryBadge, songs, scanKind, scanSummary } from './common.js';

const INTERVALS = [['0', 'Never'], ['900', 'Every 15 minutes'], ['1800', 'Every 30 minutes'], ['3600', 'Every hour'], ['21600', 'Every 6 hours'], ['86400', 'Every day']];

export async function render(ctx) {
  const [id] = ctx.args;
  if (id === 'add') return addLibrary(ctx);
  if (id) return libraryDetail(ctx, id);
  return libraryList(ctx);
}

async function libraryList(ctx) {
  const [{ libraries }, imports, duplicates] = await Promise.all([
    call('GET /admin/libraries'),
    call('GET /admin/imports', { query: { limit: 10 } }).catch(() => ({ imports: [] })),
    call('GET /admin/duplicates', { query: { limit: 20 } }).catch(() => ({ groups: [] })),
  ]);
  const head = pageHead('Libraries', { actions: h('a.btn.primary', { href: '#/libraries/add' }, 'Add a library') });
  if (!libraries.length) {
    return h('div.page', head, list(empty('No music yet', 'A library is a folder of music on the server. Trackside reads it and never changes your files unless you allow it.', h('a.btn.primary', { href: '#/libraries/add' }, 'Add a library'))));
  }
  return h('div.page',
    head,
    section('Folders', list(libraries.map((lib) => row({
      icon: lib.state === 'unavailable' ? 'warning' : 'library',
      tint: lib.state === 'unavailable' ? 'warn' : 'blue',
      title: [lib.name, libraryBadge(lib)],
      sub: [h('span.mono', lib.root), `, ${songs(lib.song_count)}`],
      href: `#/libraries/${encodeURIComponent(lib.id)}`,
    })), { iconInset: true })),
    targetsSection(ctx, libraries),
    imports.imports.length ? importsSection(ctx, imports) : null,
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
  return section('Where new songs go', h('div.list',
    rowFor('Uploaded songs', 'Files people upload from Tracks', uploads),
    acquire ? rowFor('Requested songs', 'Songs Trackside finds for requests', acquire) : null),
  { foot: writable.length ? null : 'Only a library that allows adding songs can take them. Turn that on in a library\'s settings.' });
}

const IMPORT_STATE = { receiving: ['Uploading', ''], staged: ['Checking', ''], imported: ['Added', 'good'], duplicate: ['Already there', ''], rejected: ['Not added', 'warn'], failed: ['Failed', 'bad'] };
const IMPORT_FILTERS = [['', 'All'], ['imported', 'Added'], ['rejected', 'Not added'], ['failed', 'Failed']];

function importRow(i) {
  const [label, kind] = IMPORT_STATE[i.state] || [i.state, ''];
  return row({
    icon: 'download', tint: 'muted',
    title: i.file_name,
    sub: [[i.source === 'inbox' ? 'From the inbox folder' : `Uploaded by ${i.user?.display_name || 'someone'}`, agoInline(i.created_at)].join(', '), i.error ? h('div', i.error) : null],
    trail: badge(label, kind),
  });
}

/** Recent uploads, filtered by outcome and paged with the server's cursor. */
function importsSection(ctx, first) {
  const body = h('div.form.tight');
  const more = button('Show more', {});
  let cursor = first.next_cursor;
  let state = '';
  let rows = list(first.imports.map(importRow), { iconInset: true });
  body.append(rows, h('div.btn-row', more));
  more.hidden = !cursor;
  const load = async (reset) => {
    const query = { limit: 10, state: state || undefined };
    if (!reset) query.cursor = cursor;
    try {
      const page = await call('GET /admin/imports', { query });
      cursor = page.next_cursor;
      if (reset) {
        const next = list(page.imports.length ? page.imports.map(importRow) : [empty('Nothing here')], { iconInset: true });
        rows.replaceWith(next);
        rows = next;
      } else {
        rows.append(...page.imports.map(importRow));
      }
      more.hidden = !cursor;
    } catch (err) { failed(err); }
  };
  more.addEventListener('click', () => busy(more, () => load(false)));
  const filter = segmented(nextId('imp'), IMPORT_FILTERS, '', (v) => { state = v; load(true); }, 'Show uploads');
  filter.classList.add('compact');
  return section('Recent uploads', body, { aside: filter });
}

function duplicatesSection(groups, libraries) {
  const lib = (id) => libraries.find((l) => l.id === id);
  return section('Duplicates', list(groups.map((g) => h('div.row',
    icon('copy', 'tint-muted'),
    h('div.row-main',
      h('div.row-title', `${g.songs[0].title}, ${g.songs[0].artist}`),
      g.songs.map((s) => h('div.row-sub',
        lib(s.library_id) ? h('a', { href: `#/libraries/${encodeURIComponent(s.library_id)}` }, lib(s.library_id).name) : 'Unknown library',
        ': ', h('span.mono', s.path)))))), { iconInset: true }),
  { foot: 'The same file is in more than one place. Delete the copy you do not want from the folder on disk and the next scan tidies up.' });
}

/* ---------- Add ---------- */

async function addLibrary(ctx) {
  const name = input({ autocomplete: 'off', required: true, maxlength: '64' });
  const root = input({ autocomplete: 'off', spellcheck: 'false', required: true, mono: true, placeholder: '/music' });
  const writable = checkbox('Allow adding songs', false, { sub: 'Uploads and requests can save new files into this folder' });
  const watch = checkbox('Pick up new files straight away', true, { sub: 'Otherwise new files appear on the next scan' });
  const grant = checkbox('Everyone can see it', true, { sub: 'You can change who sees it per person later' });
  const error = formError();
  const submit = button('Add library', { kind: 'primary', type: 'submit' });
  const form = h('form.form', {
    novalidate: true,
    on: {
      submit: async (event) => {
        event.preventDefault();
        error.hide();
        setFieldError(name, '');
        setFieldError(root, '');
        if (!name.value.trim()) { setFieldError(name, 'Give the library a name.'); name.focus(); return; }
        if (!root.value.trim().startsWith('/') && !/^[A-Za-z]:[\\/]/.test(root.value.trim())) { setFieldError(root, 'Use the full path from the top of the server, like /music.'); root.focus(); return; }
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
    field('Folder', root, { hint: 'The path on the server, or inside the container if you use Docker' })),
  h('div.panel.form', writable, watch, grant),
  error.el,
  h('div.btn-row.end', h('a.btn', { href: '#/libraries' }, 'Cancel'), submit));
  return h('div.page.narrow', pageHead('Add a library', { crumb: ['Libraries', '#/libraries'] }), form);
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
    return h('div.page', pageHead('Library not found', { crumb: ['Libraries', '#/libraries'] }), list(empty('This library no longer exists', null, h('a.btn', { href: '#/libraries' }, 'See all libraries'))));
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
        h('p.crumb', h('a', { href: '#/libraries' }, 'Libraries')),
        h('h1', { tabindex: '-1', 'data-page-title': '' }, lib.name),
        h('p.lede', h('span.mono', lib.root), `, ${songs(lib.song_count)}`)),
      h('div.actions', scanButton('full', 'Reread every file', false), scanButton('quick', 'Check for new files', true))),
    lib.state === 'unavailable' ? h('div.callout', { role: 'status' }, icon('warning', 'tint-warn'),
      h('div.form',
        h('p', h('strong', 'Offline'), lib.unavailable_since ? ` since ${agoInline(lib.unavailable_since)}. ` : '. ', lib.unavailable_reason || ''),
        h('p.muted', 'Check that the disk or share is mounted. Its songs stay in everyone\'s apps until it is back.'))) : null,
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
  const btn = button('Scan anyway', {});
  btn.addEventListener('click', async () => {
    const ok = await confirm({
      title: 'Scan an empty folder?',
      text: 'Every song in this library will be marked missing, and disappear from apps after a while.',
      action: 'Scan anyway',
    });
    if (ok) await busy(btn, () => startScan(ctx, lib, 'full', true));
  });
  return h('div.callout', icon('warning', 'tint-warn'),
    h('div.form', h('p', 'The last scan stopped because the folder looked empty. If you really emptied it, scan anyway.'), h('div.btn-row', btn)));
}

function scansSection(ctx, lib, scans) {
  if (!scans.length) return section('Scans', list(empty('No scans yet')));
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
      return row({ icon: 'scan', tint, title: [`${scanKind(run.kind)}: ${run.state === 'done' ? scanSummary(run).toLowerCase() : { aborted: 'stopped early', failed: 'failed' }[run.state] || run.state}`], sub: run.message ? `${sub}. ${run.message}` : sub });
    }
    const toggleBtn = button(`Show ${plural(errors, 'error')}`, { kind: 'plain' });
    toggleBtn.setAttribute('aria-expanded', 'false');
    toggleBtn.addEventListener('click', async () => {
      const open = toggleBtn.getAttribute('aria-expanded') === 'true';
      toggleBtn.setAttribute('aria-expanded', String(!open));
      toggleBtn.lastChild.textContent = open ? `Show ${plural(errors, 'error')}` : 'Hide errors';
      detail.hidden = open;
      if (!open && !detail.childElementCount) {
        try {
          const full = await call('GET /admin/scans/{id}', { values: { id: run.id } });
          detail.append(...full.errors.map((e) => h('div.small', h('div.mono', e.path), h('div.muted', e.error))));
        } catch (err) { detail.append(h('p.small', describe(err))); }
      }
    });
    return h('div', row({ icon: 'scan', tint, title: `${scanKind(run.kind)}: ${scanSummary(run).toLowerCase()}`, sub, trail: toggleBtn }), detail);
  }), { iconInset: true }));
}

function problemsSection(p) {
  const rows = [
    ...p.tag_problems.map((t) => row({ icon: 'info', tint: 'muted', title: h('span.mono', t.path), sub: t.problem })),
    ...p.quarantined.map((q) => row({ icon: 'warning', tint: 'warn', title: h('span.mono', q.path), sub: `Skipped: ${q.error}` })),
    ...p.unreadable_folders.map((f) => row({ icon: 'lock', tint: 'warn', title: h('span.mono', f.path), sub: 'Trackside is not allowed to read this folder' })),
  ];
  if (!rows.length) return null;
  return section('Files with problems', list(rows, { iconInset: true }));
}

/** File watching fell back to polling: the fix is one command on the host. */
function watchLimit() {
  return h('div.callout', icon('warning', 'tint-warn'),
    h('div.form.tight',
      h('p', h('strong', 'New songs wait for the next scan.'), ' The host limits how many folders Trackside can watch. Run this on the host so new files show up within seconds:'),
      codeLine('sudo sysctl -w fs.inotify.max_user_watches=524288', 'Copy command')));
}

function settingsSection(ctx, lib) {
  const name = input({ value: lib.name, maxlength: '64' });
  const enabled = toggle(lib.enabled);
  const watch = toggle(lib.watch);
  const writable = toggle(lib.writable);
  const interval = select(INTERVALS.some(([v]) => Number(v) === lib.quick_scan_interval_s) ? INTERVALS : [...INTERVALS, [String(lib.quick_scan_interval_s), `Every ${lib.quick_scan_interval_s} seconds`]], String(lib.quick_scan_interval_s ?? 1800));
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

  const setting = (title, sub, control) => {
    control.id = nextId('ls');
    return h('div.setting-row', h('div.row-main', h('label.row-title', { for: control.id }, title), sub ? h('div.row-sub', sub) : null), h('div.control', control));
  };

  return section('Settings', h('div.list',
    setting('Name', null, name),
    setting('In use', 'Turn off to hide its songs from everyone without removing it', enabled),
    setting('Pick up new files straight away', 'Watches the folder, so new files appear within seconds', watch),
    setting('Regular check for new files', 'A quick look for added and changed files', interval),
    setting('Allow adding songs', 'Uploads and requests can save files into this folder', writable)));
}

function removeSection(ctx, lib) {
  const btn = button('Remove library', { kind: 'danger', iconName: 'trash' });
  btn.addEventListener('click', async () => {
    const ok = await confirm({
      title: `Remove ${lib.name}?`,
      text: `Its ${songs(lib.song_count)} disappear from everyone's apps. The files on disk are not touched.`,
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

