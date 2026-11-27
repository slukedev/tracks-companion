import { call } from '../api.js';
import {
  h, icon, section, list, row, kv, bytes, ago, when, time, date, full, dayLabel, num, empty, button, busy, toast, failed,
  codeLine, meter, duration, plural,
} from '../ui.js';
import { libraryBadge, songs, scanSummary, warnIcon } from './common.js';
import { sentence } from './history.js';
import { listeningChart, streamsChart, storageChart } from './stats.js';

const INOTIFY_COMMAND = 'sudo sysctl -w fs.inotify.max_user_watches=524288';

export async function render(ctx) {
  const [overview, audit, users, ready, urls] = await Promise.all([
    ctx.take('overview') || call('GET /admin/overview'),
    call('GET /admin/audit', { query: { limit: 10 } }).catch(() => ({ entries: [] })),
    call('GET /admin/users').catch(() => ({ users: [] })),
    call('GET /readyz').catch(() => null),
    call('GET /admin/server/urls').catch(() => ({ urls: [] })),
  ]);
  const names = new Map(users.users.map((u) => [u.username, u.display_name || u.username]));

  const problems = [
    ...overview.problems,
    ...(overview.storage.warnings || []).map((message) => ({ severity: 'warning', code: 'storage', message })),
    ...proxyProblem(ready),
    ...(overview.backups.last_error ? [{ severity: 'warning', code: 'backup_failed', help: overview.backups.last_error }] : []),
  ];
  const urgent = problems.filter((p) => p.severity !== 'info');

  const head = h('header.page-head',
    h('div',
      h('h1', { tabindex: '-1', 'data-page-title': '' }, 'Overview'),
      h('p.status-line',
        h(`span.dot.${urgent.length ? 'warn' : 'good'}`),
        h('strong', urgent.length ? `${urgent.length === 1 ? 'One thing needs' : `${num(urgent.length)} things need`} attention` : 'Everything is working'),
        h('span', `${overview.server.name}, Trackside ${overview.server.version}`))));

  // DOM order is the phone order: problems, then what explains them, then the facts.
  return h('div.page',
    head,
    problems.length ? problemSection(problems, ctx, overview, urgent.length > 0) : null,
    gettingStarted(overview),
    h('div.overview-grid',
      h('div.col',
        later(ctx, 'Playback', streamsChart, 'playback'),
        historySection(overview, audit.entries.filter((e) => ctx.acquisition || !/^acqui/.test(e.action)), names),
        later(ctx, 'Listening', listeningChart, 'listening')),
      h('div.facts',
        overview.libraries.length ? librariesSection(overview) : null,
        storageSection(overview, ctx),
        serverSection(overview, urls.urls, ctx))));
}

/** Shown on a new server until there is music, a paired device and a second person. */
function gettingStarted(overview) {
  const steps = [
    [overview.libraries.length > 0, 'Add your music', '#/libraries/add', 'Add a library'],
    [overview.users.devices > 0, 'Connect your phone', '#/account/pair', 'Show pairing code'],
    [overview.users.count > 1, 'Invite someone', '#/invites/new', 'Create an invite'],
  ];
  const done = steps.filter(([d]) => d).length;
  if (done === steps.length) return null;
  return section('Get started', list([
    h('div.steps-meter', h('span.num', `${done} of ${steps.length} done`), meter(done, steps.length, 'Setup progress', 'ink')),
    ...steps.map(([finished, title, link, action]) => h('div.row',
      icon(finished ? 'check' : 'plus', finished ? 'tint-good' : 'tint-muted'),
      h('div.row-main', h('div.row-title', title)),
      h('div.row-trail', finished ? h('span.small', 'Done') : h('a.btn', { href: link }, action)))),
  ], { iconInset: true }));
}

/** /readyz notes a proxy whose forwarding headers are ignored; only that note is new here. */
function proxyProblem(ready) {
  const note = ready?.notes?.find((n) => n.includes('trusted_proxies'));
  return note ? [{ severity: 'warning', code: 'proxy_untrusted' }] : [];
}

/**
 * Plain-language wording and the fix for each problem the overview knows. The server's own
 * title and help are kept for anything not listed here.
 */
function explain(p, ctx, overview) {
  const target = p.target?.kind === 'library' ? overview.libraries.find((l) => l.id === p.target.id) : null;
  switch (p.code) {
    case 'library_unavailable': {
      const lib = target;
      return {
        title: p.title,
        help: p.help || p.message,
        fix: lib ? h('a.btn', { href: `#/libraries/${encodeURIComponent(lib.id)}` }, `Check ${lib.name}`) : h('a.btn', { href: '#/libraries' }, 'Check libraries'),
      };
    }
    case 'inotify_limit':
      return {
        title: 'New songs wait for the next scan',
        help: 'The host limits how many folders Trackside can watch. Run this on the host so new files show up within seconds:',
        fix: codeLine(INOTIFY_COMMAND, 'Copy command'),
      };
    case 'proxy_untrusted':
      return {
        title: 'Everyone shares one address',
        help: 'Requests come through a proxy Trackside does not trust yet, so sign-in limits and the activity log see one address for everyone, and signing in or pairing through it is refused. Add the proxy\'s address to this setting in the config file:',
        fix: codeLine('server.trusted_proxies', 'Copy setting name'),
      };
    case 'no_ffmpeg':
      return {
        title: 'Songs play only in their original format',
        help: 'Trackside cannot find ffmpeg, so it cannot convert songs for mobile data or older apps. Use the standard image, or install ffmpeg and restart Trackside.',
      };
    case 'backup_failed': {
      const now = button('Back up now', { iconName: 'backups' });
      now.addEventListener('click', () => busy(now, async () => {
        try {
          await call('POST /admin/backups');
          toast('Backup started');
        } catch (err) { failed(err); }
      }));
      return { title: 'The last backup failed', help: p.help, fix: now };
    }
    case 'storage':
      return {
        title: 'Running low on disk space',
        help: p.message,
        fix: h('a.btn', { href: '#/settings' }, 'Lower cache limits'),
      };
    default: {
      const page = /^(acquire|provider)/.test(p.code || '') && ctx.acquisition ? ['requests/setup', 'Check the source']
        : /^backup/.test(p.code || '') ? ['backups', 'Check backups']
          : /^(library|scan)/.test(p.code || '') ? ['libraries', 'Check libraries'] : null;
      return {
        title: p.title || p.message,
        help: [p.title ? p.message : null, p.help].filter(Boolean).join(' '),
        fix: page ? h('a.btn', { href: `#/${page[0]}` }, page[1]) : null,
      };
    }
  }
}

function problemSection(problems, ctx, overview, urgent) {
  return section(urgent ? 'Needs attention' : 'Worth knowing', list(problems.map((p) => {
    const { title, help, fix } = explain(p, ctx, overview);
    return h('div.problem',
      warnIcon(p.severity),
      h('div.row-main',
        h('div.row-title', title),
        help ? h('p.help', help) : null,
        fix ? h('div.fix', fix) : null));
  }), { iconInset: true }));
}

/* ---------- History: the five latest events ---------- */

function historyItems(overview, entries, names) {
  const items = entries.map((e) => ({ at: e.at, ...sentence(e, names) }));
  for (const lib of overview.libraries) {
    const run = lib.last_scan;
    if (!run?.finished_at) continue;
    const text = run.state === 'done' ? `Scanned ${lib.name}: ${shortSummary(run).toLowerCase()}` : `A scan of ${lib.name} stopped early`;
    items.push({ at: run.finished_at, icon: 'scan', tint: run.state === 'done' ? 'blue' : 'warn', text, full: run.message });
  }
  const last = overview.backups.last;
  if (last && !entries.some((e) => e.action === 'backup.create' && e.target === last.name)) {
    items.push({ at: last.at, icon: 'backups', tint: 'teal', text: `Backed up the server, ${bytes(last.bytes)}` });
  }
  return items.sort((a, b) => Date.parse(b.at) - Date.parse(a.at)).slice(0, 5);
}

const shortSummary = (run) => scanSummary(run).split(', ').slice(0, 2).join(', ');

function historySection(overview, entries, names) {
  const items = historyItems(overview, entries, names);
  if (!items.length) return section('Recent activity', list(empty('Nothing has happened yet')));
  const sec = section('Recent activity', list(items.map((item) => h('div.row',
    icon(item.icon, `tint-${item.tint}`),
    h('div.row-main', h('div.row-title.history-text', { title: item.full || undefined }, item.text)),
    h('div.row-trail.num', h('time', { datetime: item.at, title: full(item.at) }, shortWhen(item.at))))), { iconInset: true }),
  { aside: h('a', { href: '#/activity' }, 'See all activity') });
  sec.classList.add('history');
  return sec;
}

const shortWhen = (iso) => (dayLabel(iso) === 'Today' ? time(iso) : dayLabel(iso) === 'Yesterday' ? 'Yesterday' : date(iso));

/* ---------- Facts ---------- */

function librariesSection(overview) {
  const total = overview.libraries.reduce((a, l) => a + (l.song_count || 0), 0);
  return section('Libraries', list(overview.libraries.map((lib) => row({
    icon: lib.state === 'unavailable' ? 'warning' : 'library',
    tint: lib.state === 'unavailable' ? 'warn' : 'blue',
    title: [lib.name, libraryBadge(lib)],
    trail: h('span.num.small', num(lib.song_count)),
    href: `#/libraries/${encodeURIComponent(lib.id)}`,
    label: `${lib.name}, ${songs(lib.song_count)}${lib.state === 'unavailable' ? ', offline' : ''}`,
  })), { iconInset: true }), { aside: h('span.num', songs(total)) });
}

/** Free space appears once: in the chart's figure when there is a chart, otherwise as a line. */
function storageSection(overview, ctx) {
  const st = overview.storage;
  const sizes = h('dl',
    kv('Database', bytes((st.database_bytes || 0) + (st.wal_bytes || 0))),
    kv('Cache', bytes(st.cache_bytes ?? overview.media?.transcode_cache_bytes ?? 0)));
  const chartSlot = h('div', h('div.skeleton-row', h('span.skeleton.w25'), h('span.skeleton.block')));
  const body = h('div.list', chartSlot, sizes);
  storageChart({ free: st.data_free_bytes }).then((node) => {
    if (!ctx.isCurrent()) return;
    chartSlot.replaceChildren(node || h('dl', kv('Free space', bytes(st.data_free_bytes))));
  }).catch(() => chartSlot.replaceChildren(h('dl', kv('Free space', bytes(st.data_free_bytes)))));
  return section('Storage', body, { aside: h('a', { href: '#/settings' }, 'Cache limits') });
}

/**
 * A section whose chart arrives after the page has rendered. If the server has no stats
 * endpoint or nothing to show, the section never appears.
 */
function later(ctx, title, load, id) {
  const slot = h('div.list', { 'aria-busy': 'true' }, h('div.skeleton-row', h('span.skeleton.w25'), h('span.skeleton.block')));
  const sec = section(title, slot, { id: `sec-${id}` });
  load().then((node) => {
    if (!ctx.isCurrent()) return;
    if (!node) { sec.remove(); return; }
    slot.removeAttribute('aria-busy');
    slot.replaceChildren(node);
  }).catch(() => sec.remove());
  return sec;
}

/**
 * Songs played without keeping them: how full that space is. The meter follows whichever limit
 * is closer, songs or bytes; playing songs can push it past a limit for a while.
 */
function fetchedRow(f) {
  const ratio = (value, limit) => (limit > 0 ? value / limit : value > 0 ? Infinity : 0);
  const worst = Math.max(ratio(f.songs ?? 0, f.keep ?? 0), ratio(f.bytes ?? 0, f.max_bytes ?? 0));
  const over = worst > 1;
  const note = over ? 'Over the limit while songs are playing' : f.fetching ? `${num(f.fetching)} downloading` : null;
  const bar = meter(Math.min(worst, 1), 1, 'Space for fetched songs', over ? 'warn' : 'ink');
  return h(`div.kv.fetched${over ? '.over' : ''}`,
    h('dt', 'Fetched songs'),
    h('dd', `${num(f.songs ?? 0)} of ${num(f.keep ?? 0)} · ${bytes(f.bytes ?? 0)} of ${bytes(f.max_bytes ?? 0)}`, note ? h('span.sub', note) : null),
    bar);
}

function serverSection(overview, urls, ctx) {
  const b = overview.backups;
  const s = overview.server;
  const ffmpeg = overview.media?.ffmpeg;
  const transcodes = overview.media?.transcodes;
  const acq = overview.acquisition;
  const address = urls.find((u) => u.reachable && u.kind === 'lan') || urls.find((u) => u.reachable) || urls[0];
  const rows = [
    address ? kv('Address', h('span.mono', address.url), urls.length > 1 ? h('a', { href: '#/settings' }, `${plural(urls.length - 1, 'other address', 'other addresses')}`) : null) : null,
    kv('Last backup', b.last ? ago(b.last.at) : 'None yet', b.next_at ? `Next ${when(b.next_at)}` : 'Automatic backups are off'),
    kv('Converting songs', ffmpeg?.found
      ? (transcodes?.running ? `${num(transcodes.running)} now${transcodes.queued ? `, ${num(transcodes.queued)} waiting` : ''}` : 'Ready')
      : 'Not available', ffmpeg?.found && ffmpeg.version ? `ffmpeg ${ffmpeg.version}` : null),
    acq && ctx.acquisition ? kv('Requests', acq.enabled ? `${num(acq.active ?? 0)} in progress` : 'Turned off', acq.enabled ? `${num(acq.today ?? 0)} today` : null) : null,
    acq?.fetched && ctx.acquisition ? fetchedRow(acq.fetched) : null,
    s.uptime_s ? kv('Running for', duration(s.uptime_s), s.started_at ? `Since ${date(s.started_at)}` : null) : null,
  ];
  return section('Server', h('dl.list', rows), { aside: h('a', { href: '#/backups' }, 'Backups') });
}
