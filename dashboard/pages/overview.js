import { call } from '../api.js';
import { h, icon, section, list, row, kv, bytes, ago, time, date, full, dayLabel, num, empty } from '../ui.js';
import { libraryBadge, songs, scanSummary, warnIcon } from './common.js';
import { sentence } from './history.js';
import { listeningChart, streamsChart, storageChart } from './stats.js';

const PROBLEM_PAGE = [
  [/^(library|inotify|scan|watch)/, 'libraries', 'Libraries'],
  [/^backup/, 'backups', 'Backups'],
  [/^(acquire|provider)/, 'requests', 'Requests'],
  [/^(secret|key|device)/, 'devices', 'Devices'],
  [/^proxy/, 'settings', 'Settings'],
];

export async function render(ctx) {
  const [overview, audit, users, ready] = await Promise.all([
    call('GET /admin/overview'),
    call('GET /admin/audit', { query: { limit: 10 } }).catch(() => ({ entries: [] })),
    call('GET /admin/users').catch(() => ({ users: [] })),
    call('GET /readyz').catch(() => null),
  ]);
  const names = new Map(users.users.map((u) => [u.username, u.display_name || u.username]));

  const problems = [
    ...overview.problems,
    ...(overview.storage.warnings || []).map((message) => ({ severity: 'warning', code: 'storage', title: 'Low on space', message })),
    ...proxyProblem(ready),
    ...(overview.backups.last_error ? [{ severity: 'warning', code: 'backup_failed', title: 'Last backup failed', help: overview.backups.last_error }] : []),
  ];
  const urgent = problems.filter((p) => p.severity !== 'info');

  const head = h('header.page-head',
    h('div',
      h('h1.overview-title', { tabindex: '-1', 'data-page-title': '' }, overview.server.name),
      h('p.status-line',
        h(`span.dot.${urgent.length ? 'warn' : 'good'}`),
        h('span', urgent.length ? `${urgent.length} need attention` : 'All good'))));

  return h('div.page',
    head,
    gettingStarted(overview),
    problems.length ? problemSection(problems, ctx, urgent.length > 0) : null,
    h('div.overview-grid',
      h('div.col',
        later(ctx, 'Listening', listeningChart, 'listening'),
        later(ctx, 'Last 24 Hours', streamsChart, 'playback'),
        historySection(overview, audit.entries.filter((e) => ctx.acquisition || !/^acqui/.test(e.action)), names)),
      h('div.facts',
        overview.libraries.length ? librariesSection(overview) : null,
        freeSpaceSection(overview, ctx),
        serverSection(overview))));
}

/** Shown on a new server until there is music, a paired device and a second person. */
function gettingStarted(overview) {
  const steps = [
    [overview.libraries.length > 0, 'Add your music', '#/libraries/add', 'Add Library'],
    [overview.users.devices > 0, 'Pair a device', '#/account/pair', 'Pair'],
    [overview.users.count > 1, 'Invite someone', '#/invites/new', 'Invite'],
  ];
  if (steps.every(([done]) => done)) return null;
  return section('Get Started', list(steps.map(([done, title, link, action]) => h('div.row',
    icon(done ? 'check' : 'plus', done ? 'tint-good' : 'tint-accent'),
    h('div.row-main', h('div.row-title', title)),
    h('div.row-trail', done ? h('span.small', 'Done') : h('a.btn', { href: link }, action)))), { iconInset: true }));
}

/** /readyz notes a proxy whose forwarding headers are ignored; only that note is new here. */
function proxyProblem(ready) {
  const note = ready?.notes?.find((n) => n.includes('trusted_proxies'));
  return note ? [{ severity: 'warning', code: 'proxy_untrusted', title: 'Proxy not trusted', message: 'Add it to server.trusted_proxies in the config file.', help: note }] : [];
}

/** One line per problem and the button that fixes it; the explanation waits behind Why. */
function problemSection(problems, ctx, urgent) {
  return section(urgent ? 'Needs Attention' : 'Note', list(problems.map((p) => {
    const target = PROBLEM_PAGE.find(([re]) => re.test(p.code || ''));
    const allowed = target && (target[1] !== 'requests' || ctx.acquisition);
    const why = p.help || (p.title && p.message) ? h('details.why',
      h('summary', { 'aria-label': `Why: ${p.title || p.message}` }, 'Why'),
      h('p.help', p.title ? p.message : null, p.title && p.help ? ' ' : null, p.help || null)) : null;
    return h('div.problem',
      warnIcon(p.severity),
      h('div.row-main', h('div.row-title', p.title || p.message), why),
      allowed ? h('a.btn', { href: `#/${target[1]}`, 'aria-label': `Open ${target[2]}` }, target[2]) : null);
  }), { iconInset: true }));
}

/* ---------- History: the five latest events ---------- */

function historyItems(overview, entries, names) {
  const items = entries.map((e) => ({ at: e.at, ...sentence(e, names) }));
  for (const lib of overview.libraries) {
    const run = lib.last_scan;
    if (!run?.finished_at) continue;
    const text = run.state === 'done' ? `Scan of ${lib.name}: ${shortSummary(run)}` : `Scan of ${lib.name} stopped`;
    items.push({ at: run.finished_at, icon: 'scan', tint: run.state === 'done' ? 'blue' : 'warn', text, full: run.message });
  }
  const last = overview.backups.last;
  if (last && !entries.some((e) => e.action === 'backup.create' && e.target === last.name)) {
    items.push({ at: last.at, icon: 'backups', tint: 'teal', text: `Backup, ${bytes(last.bytes)}` });
  }
  return items.sort((a, b) => Date.parse(b.at) - Date.parse(a.at)).slice(0, 5);
}

const shortSummary = (run) => scanSummary(run).split(', ').slice(0, 2).join(', ');

function historySection(overview, entries, names) {
  const items = historyItems(overview, entries, names);
  if (!items.length) return section('History', list(empty('Nothing yet')));
  const sec = section('History', list(items.map((item) => h('div.row',
    icon(item.icon, `tint-${item.tint}`),
    h('div.row-main', h('div.row-title.history-text', { title: item.full || undefined }, item.text)),
    h('div.row-trail.num', h('time', { datetime: item.at, title: full(item.at) }, when24(item.at))))), { iconInset: true }),
  { aside: h('a', { href: '#/activity', 'aria-label': 'All activity' }, 'All') });
  sec.classList.add('history');
  return sec;
}

const when24 = (iso) => (dayLabel(iso) === 'Today' ? time(iso) : dayLabel(iso) === 'Yesterday' ? 'Yesterday' : date(iso));

/* ---------- Facts ---------- */

function librariesSection(overview) {
  return section('Libraries', list(overview.libraries.map((lib) => row({
    icon: lib.state === 'unavailable' ? 'warning' : 'library',
    tint: lib.state === 'unavailable' ? 'warn' : 'blue',
    title: lib.name,
    trail: [libraryBadge(lib), h('span.num.small', num(lib.song_count))],
    href: `#/libraries/${encodeURIComponent(lib.id)}`,
    label: `${lib.name}, ${songs(lib.song_count)}${lib.state === 'unavailable' ? ', offline' : ''}`,
  })), { iconInset: true }));
}

/** Free space appears once: in the chart's figure when there is a chart, otherwise as a line. */
function freeSpaceSection(overview, ctx) {
  const free = bytes(overview.storage.data_free_bytes);
  const body = h('div.list', h('dl', kv('Free', free)));
  storageChart().then((node) => {
    if (node && ctx.isCurrent()) body.replaceChildren(node);
  }).catch(() => {});
  return section('Storage', body);
}

/**
 * A section whose chart arrives after the page has rendered. If the server has no stats
 * endpoint or nothing to show, the section never appears.
 */
function later(ctx, title, load, id) {
  const slot = h('div.list', { 'aria-busy': 'true' }, h('div.skeleton-row', h('span.skeleton.w40'), h('span.skeleton.chart-skel')));
  const sec = section(title, slot, { id: `sec-${id}` });
  load().then((node) => {
    if (!ctx.isCurrent()) return;
    if (!node) { sec.remove(); return; }
    slot.removeAttribute('aria-busy');
    slot.replaceChildren(node);
  }).catch(() => sec.remove());
  return sec;
}

function serverSection(overview) {
  const b = overview.backups;
  return section('Server', h('dl.list',
    kv('Version', overview.server.version),
    kv('Last backup', b.last ? ago(b.last.at) : 'None')));
}
