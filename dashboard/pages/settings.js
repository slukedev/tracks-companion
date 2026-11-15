import { call, describe } from '../api.js';
import { h, icon, pageHead, section, list, row, button, busy, input, select, toggle, nextId, toast, formError, num } from '../ui.js';

const DURATIONS = (extra) => [['off', 'Off'], ['15m', '15 minutes'], ['30m', '30 minutes'], ['1h', '1 hour'], ['6h', '6 hours'], ['1d', '1 day'], ['7d', '7 days'], ...extra];
const PROFILES = [['aac-256', 'AAC 256 kbps'], ['aac-128', 'AAC 128 kbps'], ['mp3-320', 'MP3 320 kbps'], ['mp3-128', 'MP3 128 kbps'], ['opus-128', 'Opus 128 kbps'], ['flac', 'Original quality (FLAC)']];
const GRACE = [['7d', '7 days'], ['30d', '30 days'], ['90d', '90 days'], ['365d', '1 year']];
const LASTFM_KEYS = 'https://www.last.fm/api/account/create';

/** Each entry: key, title, help, kind. Keys under acquire. and enrichment.musicbrainz live on the Requests page. */
const GROUPS = [
  ['Server', [
    ['server.name', 'Server name', null, 'text'],
    ['server.public_urls', 'Other addresses', 'One per line', 'lines'],
    ['discovery.mdns', 'Local discovery', null, 'bool'],
  ]],
  ['Scanning', [
    ['scanner.watch', 'Watch folders', null, 'bool'],
    ['scanner.quick_interval', 'Quick scan', null, DURATIONS([])],
    ['scanner.full_interval', 'Full scan', null, DURATIONS([])],
    ['scanner.missing_grace', 'Forget missing', null, GRACE],
  ]],
  ['Playback', [
    ['media.cellular_profile', 'Mobile quality', null, PROFILES],
    ['media.transcode_cache_max', 'Transcode cache', '10GiB', 'size'],
    ['media.art_cache_max', 'Artwork cache', '2GiB', 'size'],
  ]],
  ['Sign-in', [
    ['auth.legacy_subsonic', 'Legacy apps', null, 'bool'],
    ['uploads.enabled', 'Uploads', null, 'bool'],
  ]],
  ['Backups', [
    ['backup.schedule', 'Schedule', 'daily 03:30', 'text'],
    ['backup.keep', 'Keep', null, 'number'],
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
  const save = button('Save Changes', { kind: 'primary', type: 'submit' });
  const discard = button('Discard', { onClick: () => ctx.rerender() });
  const bar = h('div.btn-row.end.sticky-save', { hidden: true }, discard, save);

  const encode = (kind, value) => (kind === 'lines' ? (value || []).join('\n') : value);
  const decode = (kind, raw) => {
    if (kind === 'lines') return raw.split('\n').map((s) => s.trim()).filter(Boolean);
    if (kind === 'number') return Number(raw);
    return raw;
  };

  const changed = () => {
    const out = {};
    for (const [key, { kind, read, initial }] of controls) {
      const value = read();
      if (JSON.stringify(value) !== JSON.stringify(initial)) out[key] = value;
    }
    return out;
  };
  const guard = (event) => { event.preventDefault(); event.returnValue = ''; };
  const refresh = () => {
    const dirty = Object.keys(changed()).length > 0;
    bar.hidden = !dirty;
    if (dirty) window.addEventListener('beforeunload', guard); else window.removeEventListener('beforeunload', guard);
  };
  ctx.onLeave(() => window.removeEventListener('beforeunload', guard));

  const settingRow = ([key, title, example, kind]) => {
    const entry = settings[key];
    if (!entry) return null;
    const id = nextId('set');
    const locked = !entry.editable;
    let control;
    let read;
    let extra = null;
    let sub = null;
    if (kind === 'secret') {
      ({ control, read, extra, sub } = secretControl(entry, id, locked, enrichment, refresh));
    } else if (kind === 'bool') {
      control = toggle(Boolean(entry.value), { id, disabled: locked });
      read = () => control.checked;
    } else if (Array.isArray(kind)) {
      const options = kind.some(([v]) => v === entry.value) ? kind : [...kind, [entry.value, String(entry.value)]];
      control = select(options, entry.value, { id, disabled: locked });
      read = () => control.value;
    } else if (kind === 'lines') {
      control = h('textarea.input.mono', { id, rows: '3', disabled: locked, spellcheck: 'false', placeholder: example }, encode(kind, entry.value));
      read = () => decode(kind, control.value);
    } else {
      control = input({ id, value: String(entry.value ?? ''), placeholder: example || undefined, disabled: locked, type: kind === 'number' ? 'number' : 'text', min: kind === 'number' ? '1' : undefined, mono: kind === 'size' });
      if (kind === 'number') control.classList.add('short');
      read = () => decode(kind, control.value.trim());
    }
    control.addEventListener('input', refresh);
    control.addEventListener('change', refresh);
    controls.set(key, { kind, read, initial: entry.value, control });
    return h(`div.setting-row${kind === 'lines' ? '.stacked' : ''}`,
      h('div.row-main',
        h('label.row-title', { for: id }, title, locked ? h('span.badge', { title: 'Set in the config file or environment' }, icon('lock'), 'Config') : null),
        sub),
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
            ctx.rerender();
          } catch (err) {
            error.show(err.details?.reason === 'set_by_config' ? 'Set in the config file.' : describe(err));
          }
        });
      },
    },
  },
  GROUPS.map(([title, rows]) => {
    const built = rows.map(settingRow).filter(Boolean);
    if (!built.length) return null;
    const aside = title === 'Last.fm' ? h('a', { href: LASTFM_KEYS, rel: 'noreferrer' }, 'Get a key') : undefined;
    const sec = section(title, h('div.list', built), { aside });
    if (title === 'Server' && urls.urls.length) sec.append(addresses(urls.urls));
    return sec;
  }),
  error.el,
  bar);

  return h('div.page.narrow', pageHead('Settings'), form);
}

/** A write-only key: typing replaces it, Remove clears it, Test tries the typed or saved key. */
function secretControl(entry, id, locked, enrichment, refresh) {
  let cleared = false;
  const control = input({ id, type: 'password', placeholder: entry.value ? 'Saved' : '', disabled: locked, mono: true, spellcheck: 'false' });
  const read = () => (cleared ? null : control.value.trim() || entry.value);
  const sub = h('span.row-sub', enrichmentLine(enrichment));
  const say = (text) => { sub.textContent = text; };
  const test = button('Test', {
    onClick: () => busy(test, async () => {
      const typed = control.value.trim();
      try {
        const result = await call('POST /admin/enrichment/test', { body: typed ? { api_key: typed } : {} });
        say(result.ok ? 'Working' : result.detail || 'Not working');
      } catch (err) {
        say(describe(err));
      }
    }),
  });
  const remove = entry.value && !locked ? button('Remove', {
    onClick: () => { cleared = true; control.value = ''; control.placeholder = ''; remove.hidden = true; refresh(); },
  }) : null;
  control.addEventListener('input', () => { cleared = false; });
  return { control, read, sub, extra: [test, remove] };
}

function enrichmentLine(status) {
  if (!status?.configured) return '';
  const counts = `${num(status.artists)} artists, ${num(status.songs)} songs`;
  return status.last_error ? `${counts}. ${status.last_error.message}` : counts;
}

function addresses(urls) {
  return h('div.form',
    h('h3.small.muted', 'Addresses'),
    list(urls.map((u) => row({
      icon: u.reachable ? 'check' : 'warning',
      tint: u.reachable ? 'good' : 'warn',
      title: h('span.mono', u.url),
      sub: `${{ lan: 'Home', tailnet: 'Tailscale', public: 'Internet' }[u.kind] || u.kind}${u.reachable ? '' : ', unreachable'}`,
    })), { iconInset: true }));
}
