import { call, describe } from '../api.js';
import { h, icon, pageHead, section, list, row, button, busy, input, select, toggle, nextId, toast, formError, num, copyButton } from '../ui.js';

const DURATIONS = [['off', 'Never'], ['15m', 'Every 15 minutes'], ['30m', 'Every 30 minutes'], ['1h', 'Every hour'], ['6h', 'Every 6 hours'], ['1d', 'Every day'], ['7d', 'Every week']];
const PROFILES = [['aac-256', 'AAC 256 kbps'], ['aac-128', 'AAC 128 kbps'], ['mp3-320', 'MP3 320 kbps'], ['mp3-128', 'MP3 128 kbps'], ['opus-128', 'Opus 128 kbps'], ['flac', 'Original quality (FLAC)']];
const GRACE = [['7d', '7 days'], ['30d', '30 days'], ['90d', '90 days'], ['365d', '1 year']];
const LASTFM_KEYS = 'https://www.last.fm/api/account/create';

/**
 * Each entry: key, title, hint, kind. Keys under acquire. and enrichment.musicbrainz live on the
 * Requests page.
 */
const GROUPS = [
  ['Server', [
    ['server.name', 'Server name', 'Shown in Tracks and on invites', 'text'],
    ['server.public_urls', 'Extra addresses', 'Addresses Tracks can also use, one per line, such as a domain name. They appear above once saved.', 'lines'],
    ['discovery.mdns', 'Let Tracks find this server', 'Tracks on the same Wi-Fi sees it without typing an address', 'bool'],
  ]],
  ['Libraries and uploads', [
    ['scanner.watch', 'Pick up new files straight away', 'Watches library folders, so new files appear within seconds', 'bool'],
    ['scanner.quick_interval', 'Check for new files', 'A quick look for added and changed files', DURATIONS],
    ['scanner.full_interval', 'Reread every file', 'Slower; catches changes a quick check can miss', DURATIONS],
    ['scanner.missing_grace', 'Keep missing songs for', 'In case a disk was only unplugged. After this they leave everyone\'s apps.', GRACE],
    ['uploads.enabled', 'Allow uploads from Tracks', 'People with the upload permission can add their own files', 'bool'],
  ]],
  ['Playback', [
    ['media.cellular_profile', 'Quality on mobile data', 'What Tracks streams when it is not on Wi-Fi', PROFILES],
    ['media.transcode_cache_max', 'Space for converted songs', 'Like 10GiB. Converted copies are kept to play again faster.', 'size'],
    ['media.art_cache_max', 'Space for artwork', 'Like 2GiB', 'size'],
  ]],
  ['Sync', [
    ['cloud.enabled', 'Sync between devices', 'Tracks keeps history, settings, playlists and likes in step across each person\'s devices', 'bool'],
    ['cloud.quota_items', 'Items per person', 'Lowering it stops new items but keeps what is stored', 'number'],
    ['cloud.quota_bytes', 'Space per person', 'Like 1GiB', 'size'],
  ]],
  ['Other apps', [
    ['auth.legacy_subsonic', 'Older Subsonic apps', 'Lets apps like Symfonium or Feishin sign in with an app password', 'bool'],
  ]],
  ['Backups', [
    ['backup.schedule', 'Automatic backups', null, 'schedule'],
    ['backup.keep', 'Backups to keep', 'Older ones are deleted', 'number'],
  ]],
  ['Last.fm', [
    ['enrichment.lastfm_api_key', 'API key', null, 'secret'],
  ]],
];

export async function render(ctx) {
  const [settings, urls, enrichment] = await Promise.all([
    call('GET /admin/settings'),
    call('GET /admin/server/urls').catch(() => ({ urls: [] })),
    call('GET /admin/enrichment').catch(() => null),
  ]);
  const controls = new Map();
  const error = formError();
  const save = button('Save changes', { kind: 'primary', type: 'submit' });
  const discard = button('Discard', { onClick: () => ctx.rerender() });
  const count = h('span.small.muted', { role: 'status' });
  const bar = h('div.save-bar', { hidden: true }, count, h('div.btn-row', discard, save));

  const encode = (kind, value) => (kind === 'lines' ? (value || []).join('\n') : value);
  const decode = (kind, raw) => {
    if (kind === 'lines') return raw.split('\n').map((s) => s.trim()).filter(Boolean);
    if (kind === 'number') return Number(raw);
    return raw;
  };

  const changed = () => {
    const out = {};
    for (const [key, { read, initial }] of controls) {
      const value = read();
      if (JSON.stringify(value) !== JSON.stringify(initial)) out[key] = value;
    }
    return out;
  };
  const guard = (event) => { event.preventDefault(); event.returnValue = ''; };
  const refresh = () => {
    const n = Object.keys(changed()).length;
    bar.hidden = n === 0;
    count.textContent = n === 1 ? 'One unsaved change' : `${num(n)} unsaved changes`;
    if (n) window.addEventListener('beforeunload', guard); else window.removeEventListener('beforeunload', guard);
  };
  ctx.onLeave(() => window.removeEventListener('beforeunload', guard));

  const settingRow = ([key, title, hint, kind]) => {
    const entry = settings[key];
    if (!entry) return null;
    const id = nextId('set');
    let control;
    let read;
    let extra = null;
    let sub = hint ? h('span.row-sub', hint) : null;
    if (!entry.editable) {
      control = h('span.locked', { id, title: 'Set in the config file or environment, so it can only be changed there' }, icon('lock'), lockedText(entry.value, kind), h('span.visually-hidden', ', set in the config file'));
      read = () => entry.value;
    } else if (kind === 'secret') {
      ({ control, read, extra, sub } = secretControl(entry, id, enrichment, refresh));
    } else if (kind === 'schedule') {
      ({ control, read } = scheduleControl(entry.value, id));
    } else if (kind === 'bool') {
      control = toggle(Boolean(entry.value), { id });
      read = () => control.checked;
    } else if (Array.isArray(kind)) {
      const options = kind.some(([v]) => v === entry.value) ? kind : [...kind, [entry.value, String(entry.value)]];
      control = select(options, entry.value, { id });
      read = () => control.value;
    } else if (kind === 'lines') {
      control = h('textarea.input.mono', { id, rows: '3', spellcheck: 'false', placeholder: 'music.example.org' }, encode(kind, entry.value));
      read = () => decode(kind, control.value);
    } else {
      control = input({ id, value: String(entry.value ?? ''), type: kind === 'number' ? 'number' : 'text', min: kind === 'number' ? '1' : undefined, inputmode: kind === 'number' ? 'numeric' : undefined });
      if (kind === 'number' || kind === 'size') control.classList.add('short');
      read = () => decode(kind, control.value.trim());
    }
    control.addEventListener('input', refresh);
    control.addEventListener('change', refresh);
    controls.set(key, { kind, read, initial: entry.value, control });
    const label = entry.editable ? h('label.row-title', { for: control.matches('input, select, textarea') ? id : control.querySelector('select, input')?.id }, title) : h('span.row-title', title);
    return h(`div.setting-row${kind === 'lines' ? '.stacked' : ''}`,
      h('div.row-main', label, sub),
      h('div.control', control, extra));
  };

  const form = h('form.form', {
    on: {
      submit: async (event) => {
        event.preventDefault();
        error.hide();
        const body = changed();
        if (!Object.keys(body).length) return;
        await busy(save, async () => {
          try {
            await call('PATCH /admin/settings', { body });
            toast('Saved');
            window.removeEventListener('beforeunload', guard);
            ctx.rerender();
          } catch (err) {
            error.show(describe(err));
          }
        });
      },
    },
  },
  GROUPS.map(([title, rows]) => {
    const built = rows.map(settingRow).filter(Boolean);
    if (!built.length) return null;
    const aside = title === 'Last.fm' ? h('a', { href: LASTFM_KEYS, rel: 'noreferrer' }, 'Get a free key') : undefined;
    const foot = title === 'Last.fm' ? 'Adds similar artists and suggestions in Tracks.' : null;
    return section(title, h('div.list', built), { aside, foot });
  }),
  error.el,
  bar);

  return h('div.page.narrow',
    pageHead('Settings'),
    urls.urls.length ? addresses(urls.urls) : null,
    form);
}

function lockedText(value, kind) {
  if (kind === 'bool') return value ? 'On' : 'Off';
  if (Array.isArray(kind)) return kind.find(([v]) => v === value)?.[1] || String(value);
  if (kind === 'lines') return (value || []).join(', ') || 'None';
  if (kind === 'secret') return value ? 'Saved' : 'Not set';
  return String(value ?? '');
}

/** "off" or "daily HH:MM" as a choice and a time; anything else stays editable as text. */
function scheduleControl(value, id) {
  const m = /^daily (\d\d:\d\d)$/.exec(value || '');
  if (value !== 'off' && !m) {
    const control = input({ id, value: String(value ?? '') });
    return { control, read: () => control.value.trim() };
  }
  const when = select([['off', 'Off'], ['daily', 'Every day']], m ? 'daily' : 'off', { id });
  const at = input({ type: 'time', value: m ? m[1] : '03:30', 'aria-label': 'Time of day' });
  at.classList.add('time');
  const sync = () => { at.hidden = when.value === 'off'; };
  when.addEventListener('change', sync);
  sync();
  const control = h('span.btn-row', when, at);
  return { control, read: () => (when.value === 'off' ? 'off' : `daily ${at.value || '03:30'}`) };
}

/** A write-only key: typing replaces it, Remove clears it, Test tries the typed or saved key. */
function secretControl(entry, id, enrichment, refresh) {
  let cleared = false;
  const control = input({ id, type: 'password', placeholder: entry.value ? 'Saved. Type to replace it.' : 'Paste your key', mono: true, spellcheck: 'false', autocomplete: 'off' });
  const read = () => (cleared ? null : control.value.trim() || entry.value);
  const sub = h('span.row-sub', enrichmentLine(enrichment));
  const say = (text) => { sub.textContent = text; };
  const test = button('Test', {
    onClick: () => busy(test, async () => {
      const typed = control.value.trim();
      try {
        const result = await call('POST /admin/enrichment/test', { body: typed ? { api_key: typed } : {} });
        say(result.ok ? 'The key works.' : lastfmProblem(result.detail));
      } catch (err) {
        say(describe(err));
      }
    }),
  });
  const remove = entry.value ? button('Remove', {
    kind: 'danger',
    onClick: () => { cleared = true; control.value = ''; control.placeholder = 'Removed when you save'; remove.hidden = true; refresh(); },
  }) : null;
  control.addEventListener('input', () => { cleared = false; });
  return { control, read, sub, extra: [test, remove] };
}

/** Last.fm's error codes, in words a person can act on. */
function lastfmProblem(message = '') {
  const code = Number(/error (\d+)/i.exec(message)?.[1]);
  if (code === 29) return 'Last.fm asked Trackside to slow down. Lookups carry on by themselves shortly.';
  if (code === 10 || code === 26) return 'Last.fm did not accept this key. Check it was copied in full.';
  if (code === 11 || code === 16) return 'Last.fm is having trouble. Lookups carry on by themselves.';
  return message ? 'Last.fm lookups are not working right now.' : 'The key did not work.';
}

function enrichmentLine(status) {
  if (!status?.configured) return 'Not set up yet';
  const counts = `Found ${num(status.artists)} artists and ${num(status.songs)} songs so far`;
  return status.last_error ? `${counts}. ${lastfmProblem(status.last_error.message)}` : counts;
}

const KIND = { lan: 'Home network', tailnet: 'Tailscale', public: 'Internet' };

/** What people type into Tracks, first, with a Copy button each. */
function addresses(urls) {
  return section('Addresses for Tracks', list(urls.map((u) => row({
    icon: u.reachable ? 'check' : 'warning',
    tint: u.reachable ? 'good' : 'warn',
    title: h('span.mono', { translate: 'no' }, u.url),
    sub: `${KIND[u.kind] || u.kind}${u.source === 'configured' ? ', added by you' : ''}${u.reachable ? '' : '. Trackside could not reach it from here.'}`,
    trail: (() => { const b = copyButton(u.url); b.setAttribute('aria-label', `Copy ${u.url}`); return b; })(),
  })), { iconInset: true }), { foot: 'Type one of these into Tracks when it asks for a server.' });
}
