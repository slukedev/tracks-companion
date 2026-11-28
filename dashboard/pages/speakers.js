// Speakers: the connection to Music Assistant, Trackside's own source in it, and what Music
// Assistant brings (speakers and streaming services). Providers are added and signed in to in
// Music Assistant's own interface; this page links there. The token is write-only.

import { call, describe } from '../api.js';
import {
  h, icon, section, row, kv, badge, button, busy, input, toggle, select, field, setFieldError, formError,
  confirm, toast, failed, agoInline, date, plural, num, nextId, empty,
} from '../ui.js';
import { assistantState, assistantHref } from './common.js';

const EXAMPLE_URL = 'http://homeassistant.local:8095';
const WEB = /^https?:\/\//i;
/** A bare host and port gets the scheme of the example, which is what Music Assistant serves. */
const wholeAddress = (text) => {
  const value = text.trim();
  if (!value || WEB.test(value)) return value;
  return /^[a-z][a-z0-9+.-]*:\/\//i.test(value) ? value : `${new URL(EXAMPLE_URL).protocol}//${value}`;
};
const isAddress = (value) => {
  if (/\s/.test(value)) return false;
  try { return Boolean(new URL(value).hostname); } catch { return false; }
};
const NOT_ADDRESS = 'That is not an address. Write it as your browser shows it.';
const RELAY_LIMITS = [1, 2, 3, 4, 6, 8];
const TESTED_SCHEMA = 65;

export async function render(ctx) {
  // All three are asked for at once; the providers can take longer, so they fill in later.
  const connection = call('GET /admin/music-assistant');
  const registration = call('GET /admin/music-assistant/registration').catch(() => null);
  const providers = call('GET /admin/music-assistant/providers').catch((error) => ({ error }));
  const conn = await connection;
  if (!conn.configured) return setupPage(ctx);
  const reg = await registration;
  const state = conn.connection?.state || 'unreachable';
  const connected = state === 'connected';

  const parts = {};
  const page = h('div.page.narrow',
    head(conn),
    stateNote(ctx, conn, parts),
    connected ? providerSections(ctx, conn, providers) : null,
    registrationSection(ctx, conn, reg, parts),
    connectionSection(ctx, conn, parts),
    disconnectRow(ctx));

  if (state === 'connecting') watchConnecting(ctx);
  const target = ctx.args[0];
  if (target) {
    requestAnimationFrame(() => {
      if (target === 'token') parts.replaceToken?.();
      else if (target === 'connection') parts.url?.focus();
      else if (target === 'registration') page.querySelector('#sec-registration')?.closest('section')?.scrollIntoView({ block: 'start' });
    });
  }
  return page;
}

/** While Trackside connects, look again every couple of seconds and redraw once it settles. */
function watchConnecting(ctx) {
  let timer = 0;
  ctx.onLeave(() => clearTimeout(timer));
  const poll = async () => {
    try {
      const next = await call('GET /admin/music-assistant');
      if (!ctx.isCurrent()) return;
      if (next.connection?.state !== 'connecting') { ctx.rerender(); return; }
    } catch { /* look again */ }
    timer = setTimeout(poll, 2000);
  };
  timer = setTimeout(poll, 2000);
}

function openLink(url, label, { kind = '', ariaLabel } = {}) {
  const href = assistantHref(url);
  if (!href) return null;
  return h(`a.btn${kind ? `.${kind}` : ''}`, { href, rel: 'noreferrer', 'aria-label': ariaLabel }, icon('open', 'sm'), label);
}

function head(conn) {
  const [dot, label] = assistantState(conn.connection?.state);
  const server = conn.server;
  const since = conn.connection?.since;
  const detail = conn.connection?.state === 'connected'
    ? [server ? `Music Assistant ${server.version}` : 'Music Assistant', since ? `since ${agoInline(since).replace(/^today at /i, '')}` : null].filter(Boolean).join(', ')
    : server ? `Music Assistant ${server.version}` : 'Music Assistant';
  return h('header.page-head',
    h('div',
      h('h1', { tabindex: '-1', 'data-page-title': '' }, 'Speakers'),
      h('p.status-line', h(`span.dot${dot ? `.${dot}` : ''}`), h('strong', label), h('span', detail))),
    conn.ui_url && conn.connection?.state !== 'setup_required' ? h('div.actions', openLink(conn.ui_url, 'Open Music Assistant')) : null);
}

/* ---------- One sentence for each state, with its next step ---------- */

function stateNote(ctx, conn, parts) {
  const state = conn.connection?.state;
  const server = conn.server;
  const lastError = conn.connection?.last_error?.message;
  const note = (kind, iconName, title, text, action) => h(`div.callout.state-note${kind ? `.${kind}` : ''}`, { role: kind === 'bad' ? 'alert' : 'status' },
    iconName === 'live' ? h('span.dot.live.note-dot') : icon(iconName, kind === 'bad' ? 'tint-bad' : kind === 'info' ? 'tint-muted' : 'tint-warn'),
    h('div.note-text', h('p.note-title', title), text ? h('p', text) : null),
    action ? h('div.note-action', action) : null);

  switch (state) {
    case 'connecting':
      return note('info', 'live', 'Connecting to Music Assistant…', 'This takes a few seconds.');
    case 'auth_failed':
      return note('bad', 'critical', 'Music Assistant refused the token.',
        'Make a new long-lived token in Music Assistant under Settings, Profile, then replace it here.',
        button('Replace token', { kind: 'primary', onClick: () => parts.replaceToken?.() }));
    case 'incompatible': {
      const old = server && server.schema_version < TESTED_SCHEMA;
      return note('', 'warning',
        old ? `Music Assistant ${server.version} is too old for Trackside.` : `This Music Assistant no longer works with this Trackside.`,
        old ? 'Update Music Assistant to 2.10 or newer.' : 'Update Trackside to its latest version.');
    }
    case 'unreachable':
      return note('', 'warning', 'Trackside cannot reach Music Assistant.',
        [lastError, 'Check that it is running and the address below is right.'].filter(Boolean).join(' '),
        button('Test connection', { onClick: () => parts.test?.() }));
    case 'setup_required':
      return note('', 'warning', 'Music Assistant is not set up yet.', 'Open it once to make its first account, then come back here.',
        openLink(conn.ui_url, 'Open Music Assistant', { kind: 'primary' }));
    case 'disabled': {
      const on = button('Turn on', { kind: 'primary' });
      on.addEventListener('click', () => busy(on, async () => {
        try {
          await call('PATCH /admin/music-assistant', { body: { enabled: true } });
          ctx.rerender();
        } catch (err) { failed(err); }
      }));
      return note('info', 'info', 'Music Assistant is turned off.', 'Tracks shows no speakers or streaming services until you turn it on.', on);
    }
    case 'connected':
      return server && !server.tested
        ? note('info', 'info', `Music Assistant ${server.version} is newer than Trackside has been tested with.`, 'Most things work. If something does not, look for a Trackside update.')
        : null;
    default:
      return null;
  }
}

/* ---------- Speakers and streaming services, from Music Assistant ---------- */

const placeholderRows = (n) => Array.from({ length: Math.max(1, n) }, () => h('div.skeleton-row', h('span.skeleton.w40'), h('span.skeleton.w25')));

function providerSections(ctx, conn, pending) {
  const speakerList = h('div.list.inset-icon', { 'aria-busy': 'true' }, placeholderRows(2));
  const serviceList = h('div.list.inset-icon', { 'aria-busy': 'true' }, placeholderRows(conn.services ?? 2));
  const speakerAside = h('div.btn-row', h('span.num', conn.speakers !== undefined ? plural(conn.speakers, 'speaker') : ''));
  const speakers = section('Speakers', speakerList, { aside: speakerAside });
  const services = section('Streaming services', serviceList, {
    foot: 'Sign in to services in Music Assistant. Switch one off to keep it out of Tracks.',
  });

  pending.then((lists) => {
    if (!ctx.isCurrent()) return;
    speakerList.removeAttribute('aria-busy');
    serviceList.removeAttribute('aria-busy');
    if (lists.error) {
      const text = lists.error.code === 'catalog_unavailable' ? 'Music Assistant is not answering right now. These come back by themselves.' : describe(lists.error);
      speakerList.replaceChildren(empty('Not available right now', text));
      services.remove();
      return;
    }
    const add = openLink(lists.ui_url || conn.ui_url, 'Add', { kind: 'quiet', ariaLabel: 'Add speakers or services in Music Assistant' });
    const total = lists.players.reduce((n, p) => n + (p.speakers || 0), 0);
    speakerAside.replaceChildren(h('span.num', plural(total, 'speaker')), add);
    speakerList.replaceChildren(...(lists.players.length
      ? lists.players.map(playerRow)
      : [empty('No speakers yet', 'Add Sonos, AirPlay, Chromecast or others in Music Assistant. They show up here and in Tracks.', openLink(lists.ui_url || conn.ui_url, 'Open Music Assistant'))]));
    const music = lists.music.filter((m) => !m.trackside);
    serviceList.replaceChildren(...(music.length
      ? music.map((m) => serviceRow(ctx, conn, m))
      : [empty('No streaming services yet', 'Add Spotify, Tidal, Qobuz or others in Music Assistant, and they appear in Tracks for everyone allowed.')]));
  });
  return [speakers, services];
}

function playerRow(p) {
  return row({
    icon: p.available ? 'speaker' : 'warning',
    tint: p.available ? 'muted' : 'warn',
    title: p.name,
    sub: p.available ? plural(p.speakers || 0, 'speaker') : 'Not working in Music Assistant',
    trail: openLink(p.ui_url, '', { kind: 'quiet.icon-only', ariaLabel: `Open ${p.name} in Music Assistant` }),
  });
}

/** The switch changes hidden_services at once, and goes back if the server says no. */
function serviceRow(ctx, conn, m) {
  const hidden = new Set(conn.hidden_services || []);
  const sub = h('span');
  const describeRow = (isHidden) => {
    sub.textContent = !m.available ? 'Not working in Music Assistant' : !m.streaming ? 'Music in Music Assistant' : isHidden ? 'Hidden from Tracks' : 'In Tracks';
  };
  describeRow(m.hidden);
  let control = null;
  if (m.streaming) {
    control = toggle(!m.hidden, { 'aria-label': `Show ${m.name} in Tracks` });
    control.addEventListener('change', async () => {
      const show = control.checked;
      describeRow(!show);
      const current = new Set(conn.hidden_services || [...hidden]);
      if (show) current.delete(m.instance_id); else current.add(m.instance_id);
      const list = [...current];
      try {
        const saved = await call('PATCH /admin/music-assistant', { body: { hidden_services: list } });
        conn.hidden_services = saved?.hidden_services ?? list;
      } catch (err) {
        control.checked = !show;
        describeRow(show);
        failed(err);
      }
    });
  }
  return row({
    icon: m.available ? 'waveform' : 'warning',
    tint: m.available ? 'muted' : 'warn',
    title: m.name,
    sub,
    trail: [control, openLink(m.ui_url, '', { kind: 'quiet.icon-only', ariaLabel: `Open ${m.name} in Music Assistant` })],
  });
}

/* ---------- Trackside's own source in Music Assistant ---------- */

function registrationSection(ctx, conn, initial, parts) {
  const body = h('div.list.inset-icon');
  const sec = section('Your library on speakers', body, { id: 'sec-registration' });
  let timer = 0;
  let shownState = '';
  ctx.onLeave(() => clearTimeout(timer));

  const show = (reg) => {
    clearTimeout(timer);
    if (reg.state !== shownState || reg.state !== 'registering') body.replaceChildren(...view(reg));
    shownState = reg.state;
    if (reg.state === 'registering') timer = setTimeout(poll, 1500);
  };
  const poll = async () => {
    try {
      const next = await call('GET /admin/music-assistant/registration');
      if (ctx.isCurrent()) show(next);
    } catch {
      if (ctx.isCurrent()) timer = setTimeout(poll, 4000);
    }
  };

  const register = (label, kind = '') => {
    const btn = button(label, { kind });
    btn.addEventListener('click', () => busy(btn, async () => {
      try {
        show(await call('POST /admin/music-assistant/registration'));
      } catch (err) { failed(err); }
    }));
    return btn;
  };
  const unregister = () => {
    const btn = button('Remove', { kind: 'danger', ariaLabel: 'Remove your library from Music Assistant' });
    btn.addEventListener('click', async () => {
      const ok = await confirm({
        title: 'Remove your library from Music Assistant?',
        text: 'Speakers stop playing songs from Trackside, and the key Music Assistant used is revoked. You can add it again at any time.',
        action: 'Remove',
      });
      if (!ok) return;
      await busy(btn, async () => {
        try {
          await call('DELETE /admin/music-assistant/registration');
          toast('Removed from Music Assistant');
          show({ ...initial, state: 'not_registered', key: undefined });
        } catch (err) { failed(err); }
      });
    });
    return btn;
  };

  const status = (mark, title, sub, trail) => h('div.row.reg-row',
    mark,
    h('div.row-main', h('div.row-title', title), sub ? h('div.row-sub', sub) : null),
    trail ? h('div.row-trail.wrap', trail) : null);

  const view = (reg) => {
    const rows = [];
    const instance = reg.provider_instance_id;
    switch (reg.state) {
      case 'registered': {
        const key = reg.key;
        const used = key?.last_used_at ? `used ${agoInline(key.last_used_at)}` : 'not used yet';
        const live = conn.connection?.state === 'connected';
        rows.push(status(icon('check', live ? 'tint-good' : 'tint-muted'), live ? 'Plays on every speaker' : 'Registered',
          key ? h('span', 'Key ', h('span.mono', { translate: 'no' }, `${key.prefix}…`), `, made ${date(key.created_at)}, ${used}`) : null,
          [register('Register again'), unregister(), openLink(reg.ui_url || conn.ui_url, '', { kind: 'quiet.icon-only', ariaLabel: 'Open the Trackside source in Music Assistant' })]));
        break;
      }
      case 'registering':
        rows.push(status(h('span.dot.live.row-dot'), 'Adding your library to Music Assistant…', 'This takes a few seconds.'));
        break;
      case 'needs_admin':
        rows.push(status(icon('warning', 'tint-warn'), "Music Assistant needs an admin's token",
          "Only an admin's token can add your library: sign in to Music Assistant as an admin, make a long-lived token under Settings, Profile, and replace the token below.",
          button('Replace token', { kind: 'primary', onClick: () => parts.replaceToken?.() })));
        break;
      case 'failed':
        rows.push(status(icon('critical', 'tint-bad'), 'Adding your library did not work',
          reg.last_error?.message || 'Music Assistant did not accept the Trackside source.', register('Try again', 'primary')));
        break;
      case 'stale':
        rows.push(status(icon('warning', 'tint-warn'), 'Music Assistant lost your library',
          conn.auto_register ? 'Trackside puts it back by itself. You can also do it now.' : 'Add it again so speakers can play it.',
          register('Add again', 'primary')));
        break;
      default:
        rows.push(status(icon('plus', 'tint-muted'), 'Not on speakers yet', 'Add your library to Music Assistant so every speaker can play it.',
          register('Add library', 'primary')));
    }
    if (instance && reg.state === 'registered') rows[0].querySelector('.row-title').append(h('span.small.muted.mono', { translate: 'no' }, instance));
    rows.push(autoRegisterRow(ctx, conn));
    const account = reg.service_user || initial?.service_user;
    if (account?.id) {
      rows.push(row({ icon: 'library', tint: 'muted', title: 'Libraries it can play', sub: 'Chosen on its account page', href: `#/people/${encodeURIComponent(account.id)}` }));
    }
    return rows;
  };

  show(initial || { state: 'not_registered' });
  return sec;
}

function autoRegisterRow(ctx, conn) {
  const id = nextId('auto');
  const sw = toggle(conn.auto_register !== false, { id });
  sw.addEventListener('change', async () => {
    const on = sw.checked;
    try {
      await call('PATCH /admin/music-assistant', { body: { auto_register: on } });
      conn.auto_register = on;
    } catch (err) { sw.checked = !on; failed(err); }
  });
  return h('div.setting-row',
    h('div.row-main', h('label.row-title', { for: id }, 'Keep it registered'), h('span.row-sub', 'Repairs the registration when Music Assistant loses it')),
    h('div.control', sw));
}

/* ---------- Connection ---------- */

/** The result of Test connection. Its space is kept from the start, so nothing below moves. */
function testSlot(idle) {
  const el = h('div.test-result', { role: 'status', 'aria-live': 'polite' });
  const say = (mark, title, sub) => el.replaceChildren(h('div.say', mark, h('div', title ? h('p.say-title', title) : null, sub ? h('p.say-sub', sub) : null)));
  const api = {
    el,
    idle: () => say(null, '', idle),
    busy: () => say(h('span.dot.live'), 'Testing…', 'Connecting, signing in and counting speakers'),
    show(result) {
      const s = result.server;
      const facts = s ? [`Music Assistant ${s.version}`, `schema ${s.schema_version}`] : [];
      if (result.ok) {
        if (result.players !== undefined) facts.push(plural(result.players, 'player'));
        if (result.providers !== undefined) facts.push(plural(result.providers, 'provider'));
        const unsupported = s && s.compatible === false;
        say(icon(unsupported ? 'warning' : 'check', unsupported ? 'tint-warn' : 'tint-good'),
          unsupported ? 'Connected, but this version does not work with Trackside' : `Connected in ${num(result.elapsed_ms)} ms`,
          [facts.join(' · '), s && !s.tested ? 'Newer than Trackside has been tested with.' : null].filter(Boolean).join('. '));
      } else {
        say(icon('critical', 'tint-bad'), result.detail || 'Could not connect.', [facts.join(' · '), `${num(result.elapsed_ms)} ms`].filter(Boolean).join(' · '));
      }
    },
    error: (err) => say(icon('critical', 'tint-bad'), describe(err), null),
    text: (text) => say(null, '', text),
  };
  api.idle();
  return api;
}

const lockedField = (label, value) => h('div.field',
  h('span.label', label),
  h('span.locked', { title: 'Set in the config file or environment, so it can only be changed there' }, icon('lock'), value),
  h('p.hint', 'Set in the config file'));

function connectionSection(ctx, conn, parts) {
  const error = formError();
  const controls = {};
  const urlLocked = conn.source?.url === 'config';
  const tokenLocked = conn.source?.token === 'config';

  // Address
  let urlField;
  if (urlLocked) urlField = lockedField('Address', h('span.mono', { translate: 'no' }, conn.url || ''));
  else {
    controls.url = input({ type: 'url', inputmode: 'url', mono: true, value: conn.url || '', placeholder: EXAMPLE_URL, spellcheck: 'false', autocapitalize: 'none', name: 'ma-url' });
    urlField = field('Address', controls.url, { hint: 'As your browser shows it when Music Assistant is open' });
    parts.url = controls.url;
  }

  // Token: write-only. Once saved only "Saved" and when come back.
  let tokenField;
  if (tokenLocked) tokenField = lockedField('Token', conn.token_set ? 'Saved' : 'Not set');
  else {
    controls.token = input({ type: 'password', mono: true, spellcheck: 'false', autocapitalize: 'none', name: 'ma-token', autocomplete: 'off', placeholder: 'Paste a long-lived token' });
    tokenField = field('Token', controls.token, { hint: "In Music Assistant: Settings, Profile, Long-lived access tokens. Use an admin's." });
    if (conn.token_set) {
      const who = conn.user ? `, signs in as ${conn.user.display_name || conn.user.username}${conn.user.role === 'admin' ? ' (admin)' : ''}` : '';
      const replace = button('Replace', { ariaLabel: 'Replace the token' });
      const cancel = button('Keep saved token', { kind: 'quiet' });
      const refused = conn.connection?.state === 'auth_failed';
      const saved = h('div.saved-value', icon(refused ? 'critical' : 'check', refused ? 'sm tint-bad' : 'sm tint-good'),
        h('span', `Saved ${conn.token_set_at ? date(conn.token_set_at) : ''}${refused ? ', refused by Music Assistant' : who}`), replace);
      const hint = tokenField.querySelector('.hint');
      tokenField.querySelector('label').after(saved);
      controls.token.hidden = true;
      hint.hidden = true;
      cancel.hidden = true;
      hint.after(cancel);
      const editing = (on) => {
        saved.hidden = on;
        controls.token.hidden = !on;
        hint.hidden = !on;
        cancel.hidden = !on;
        if (!on) { controls.token.value = ''; setFieldError(controls.token, ''); }
      };
      replace.addEventListener('click', () => { editing(true); controls.token.focus(); });
      cancel.addEventListener('click', () => { editing(false); replace.focus(); });
      parts.replaceToken = () => { editing(true); controls.token.scrollIntoView({ block: 'center' }); controls.token.focus({ preventScroll: true }); };
    } else {
      parts.replaceToken = () => { controls.token.scrollIntoView({ block: 'center' }); controls.token.focus({ preventScroll: true }); };
    }
  }

  // Where Music Assistant reaches Trackside.
  const lan = conn.lan_url;
  controls.lan_url = input({ type: 'url', inputmode: 'url', mono: true, spellcheck: 'false', autocapitalize: 'none', name: 'ma-lan-url', value: lan?.source === 'configured' ? lan.url : '', placeholder: lan?.url || '' });
  let lanClear = false;
  const lanField = field('Address Music Assistant uses for Trackside', controls.lan_url, {
    hint: lan?.source === 'configured' ? 'Set by you.' : 'Found automatically. Change it only if speakers cannot play your library.',
  });
  if (lan?.source === 'configured') {
    const useFound = button('Use the found address', { kind: 'plain' });
    useFound.addEventListener('click', () => { lanClear = true; controls.lan_url.value = ''; controls.lan_url.placeholder = 'The found address, once you save'; useFound.hidden = true; });
    controls.lan_url.addEventListener('input', () => { lanClear = false; });
    lanField.querySelector('.hint').append(' ', useFound);
  }
  const lanNote = lan?.warning || (lan && !lan.reachable ? 'Music Assistant has not reached Trackside at this address yet.' : null);
  if (lanNote) lanField.append(h('p.field-note', icon('warning', 'sm tint-warn'), h('span', lanNote)));

  // A private certificate authority, for a secure address with its own CA.
  const caText = h('textarea.input.mono', { rows: '4', spellcheck: 'false', autocomplete: 'off', autocapitalize: 'off', name: 'ma-ca', placeholder: '-----BEGIN CERTIFICATE-----' });
  controls.ca_pem = caText;
  const caFile = h('input', { type: 'file', accept: '.pem,.crt,.cer', hidden: true });
  const chosen = h('span.small.muted', { role: 'status' });
  const choose = button('Choose file', { onClick: () => caFile.click() });
  caFile.addEventListener('change', async () => {
    const picked = caFile.files?.[0];
    if (!picked) return;
    caText.value = await picked.text();
    chosen.textContent = `${picked.name} chosen`;
    setFieldError(caText, '');
  });
  let caClear = false;
  const caBody = h('div.form.tight', field('Certificate authority', caText, { hint: 'Only for a secure address signed by your own certificate authority. Then only it is trusted.' }), h('div.btn-row', choose, chosen, caFile));
  const caSummary = h('summary', icon('chevron', 'sm disclosure-chev'), h('span', 'Private certificate'), conn.custom_ca ? badge('Saved', 'good') : null);
  if (conn.custom_ca) {
    const remove = button('Remove', { kind: 'danger', ariaLabel: 'Remove the private certificate' });
    remove.addEventListener('click', () => { caClear = true; caText.value = ''; caText.placeholder = 'Removed when you save'; remove.hidden = true; });
    caText.addEventListener('input', () => { caClear = false; });
    caBody.querySelector('.btn-row').append(remove);
    caText.placeholder = 'Saved. Paste or choose a file to replace it.';
  }
  const ca = h('details.disclosure', caSummary, caBody);

  // Test and save.
  const slot = testSlot('Test checks the address and token without saving them.');
  const test = button('Test connection');
  const runTest = () => busy(test, async () => {
    error.hide();
    slot.busy();
    const body = {};
    const url = controls.url ? wholeAddress(controls.url.value) : '';
    if (controls.url) setFieldError(controls.url, '');
    if (url && !isAddress(url)) { setFieldError(controls.url, NOT_ADDRESS); slot.idle(); return; }
    if (controls.url && url && url !== conn.url) body.url = url;
    if (controls.token?.value.trim()) body.token = controls.token.value.trim();
    try {
      slot.show(await call('POST /admin/music-assistant/test', { body }));
    } catch (err) { slot.error(err); }
  });
  test.addEventListener('click', runTest);
  parts.test = () => { test.scrollIntoView({ block: 'center' }); test.focus({ preventScroll: true }); runTest(); };

  const save = button('Save', { kind: 'primary', type: 'submit' });
  const form = h('form.panel.form', {
    novalidate: true,
    on: {
      submit: async (event) => {
        event.preventDefault();
        error.hide();
        Object.values(controls).forEach((c) => setFieldError(c, ''));
        const body = {};
        if (controls.url) {
          const url = wholeAddress(controls.url.value);
          if (!url) { setFieldError(controls.url, 'Type the address of Music Assistant.'); controls.url.focus(); return; }
          if (!isAddress(url)) { setFieldError(controls.url, NOT_ADDRESS); controls.url.focus(); return; }
          if (url !== conn.url) body.url = url;
        }
        if (controls.token?.value.trim()) body.token = controls.token.value.trim();
        const lanTyped = wholeAddress(controls.lan_url.value);
        if (lanTyped && !isAddress(lanTyped)) { setFieldError(controls.lan_url, NOT_ADDRESS); controls.lan_url.focus(); return; }
        if (lanClear) body.lan_url = null;
        else if (lanTyped && lanTyped !== (lan?.source === 'configured' ? lan.url : '')) body.lan_url = lanTyped;
        else if (!lanTyped && lan?.source === 'configured') body.lan_url = null;
        if (caClear) body.ca_pem = null;
        else if (caText.value.trim()) body.ca_pem = caText.value.trim();
        if (!Object.keys(body).length) { slot.text('Nothing has changed.'); return; }
        await busy(save, async () => {
          try {
            await call('PATCH /admin/music-assistant', { body });
            toast(body.url || body.token || 'ca_pem' in body ? 'Saved. Reconnecting…' : 'Saved');
            ctx.rerender();
          } catch (err) {
            const control = controls[err.details?.field];
            if (control) {
              if (control === caText) ca.open = true;
              setFieldError(control, describe(err));
              control.focus();
            } else error.show(describe(err));
          }
        });
      },
    },
  },
  urlField,
  tokenField,
  lanField,
  ca,
  error.el,
  h('div.test-bar', h('div.btn-row', test, save), slot.el));

  return section('Connection', [facts(conn), form, optionsList(ctx, conn)], { id: 'sec-connection' });
}

function facts(conn) {
  const s = conn.server;
  const u = conn.user;
  if (!s && !u) return null;
  return h('dl.list',
    s ? kv('Music Assistant', s.version, `Schema ${s.schema_version}${s.tested ? '' : ', newer than tested'}`) : null,
    u ? kv('Signed in as', u.display_name || u.username, u.role === 'admin' ? 'Admin' : 'Not an admin') : null);
}

function optionsList(ctx, conn) {
  const rows = [];
  const enabledId = nextId('on');
  const enabled = toggle(conn.enabled !== false, { id: enabledId });
  enabled.addEventListener('change', async () => {
    const on = enabled.checked;
    try {
      await call('PATCH /admin/music-assistant', { body: { enabled: on } });
      toast(on ? 'Turned on' : 'Turned off');
      ctx.rerender();
    } catch (err) { enabled.checked = !on; failed(err); }
  });
  rows.push(h('div.setting-row',
    h('div.row-main', h('label.row-title', { for: enabledId }, 'Use Music Assistant'), h('span.row-sub', 'Off hides speakers and streaming services in Tracks')),
    h('div.control', enabled)));

  if (conn.relays && conn.server?.relay_supported !== false) {
    const id = nextId('relay');
    const max = conn.relays.max;
    const options = RELAY_LIMITS.includes(max) ? RELAY_LIMITS : [...RELAY_LIMITS, max].sort((a, b) => a - b);
    const limit = select(options.map((n) => [n, String(n)]), max, { id });
    limit.classList.add('short');
    limit.addEventListener('change', async () => {
      const value = Number(limit.value);
      try {
        await call('PATCH /admin/music-assistant', { body: { relay_max_sessions: value } });
        conn.relays.max = value;
        toast('Saved');
      } catch (err) { limit.value = String(conn.relays.max); failed(err); }
    });
    rows.push(h('div.setting-row',
      h('div.row-main', h('label.row-title', { for: id }, 'Phones streaming at once'),
        h('span.row-sub', `Streaming services reach phones through Trackside. ${conn.relays.active ? `${num(conn.relays.active)} playing now.` : 'None playing now.'}`)),
      h('div.control', limit)));
  }
  return h('div.list', rows);
}

function disconnectRow(ctx) {
  const btn = button('Disconnect Music Assistant', { kind: 'danger', iconName: 'close' });
  btn.addEventListener('click', async () => {
    const ok = await confirm({
      title: 'Disconnect Music Assistant?',
      text: 'Trackside removes your library from it and forgets its address and token. Tracks stops showing speakers and streaming services.',
      action: 'Disconnect',
    });
    if (!ok) return;
    await busy(btn, async () => {
      try {
        await call('DELETE /admin/music-assistant');
        toast('Disconnected');
        ctx.rerender();
      } catch (err) { failed(err); }
    });
  });
  return h('div.btn-row', btn);
}

/* ---------- First run ---------- */

function setupPage(ctx) {
  const url = input({ type: 'url', inputmode: 'url', mono: true, placeholder: EXAMPLE_URL, spellcheck: 'false', autocapitalize: 'none', name: 'ma-url' });
  const token = input({ type: 'password', mono: true, spellcheck: 'false', autocapitalize: 'none', name: 'ma-token', autocomplete: 'off', placeholder: 'Paste a long-lived token' });
  const error = formError();
  const slot = testSlot('Test first if you like. Nothing is saved until you connect.');
  const connect = button('Connect', { kind: 'primary', type: 'submit' });
  const test = button('Test connection');
  const read = () => ({ url: wholeAddress(url.value), token: token.value.trim() });
  const check = ({ url: u, token: t }) => {
    setFieldError(url, '');
    setFieldError(token, '');
    if (!u) { setFieldError(url, 'Type the address of Music Assistant.'); url.focus(); return false; }
    if (!isAddress(u)) { setFieldError(url, NOT_ADDRESS); url.focus(); return false; }
    if (!t) { setFieldError(token, 'Paste a token from Music Assistant.'); token.focus(); return false; }
    return true;
  };
  test.addEventListener('click', () => {
    const body = read();
    if (!check(body)) return;
    busy(test, async () => {
      slot.busy();
      try { slot.show(await call('POST /admin/music-assistant/test', { body })); } catch (err) { slot.error(err); }
    });
  });
  const form = h('form.panel.form', {
    novalidate: true,
    on: {
      submit: async (event) => {
        event.preventDefault();
        error.hide();
        const body = read();
        if (!check(body)) return;
        await busy(connect, async () => {
          try {
            await call('PATCH /admin/music-assistant', { body: { ...body, enabled: true } });
            toast('Connecting to Music Assistant…');
            ctx.rerender();
          } catch (err) {
            const control = { url, token }[err.details?.field];
            if (control) { setFieldError(control, describe(err)); control.focus(); } else error.show(describe(err));
          }
        });
      },
    },
  },
  h('div.setup-intro',
    h('span.setup-mark', icon('speaker')),
    h('div', h('h2', 'Connect Music Assistant'),
      h('p.muted', 'Play your library on speakers around the home, and bring the streaming services you add there into Tracks.'))),
  field('Address', url, { hint: 'As your browser shows it when Music Assistant is open' }),
  field('Token', token, { hint: "In Music Assistant: Settings, Profile, Long-lived access tokens. Use an admin's, so Trackside can add your library." }),
  error.el,
  h('div.test-bar', h('div.btn-row', test, connect), slot.el));

  return h('div.page.narrow',
    h('header.page-head', h('div', h('h1', { tabindex: '-1', 'data-page-title': '' }, 'Speakers'),
      h('p.status-line', h('span.dot'), h('strong', 'Not set up'), h('span', 'Music Assistant')))),
    form);
}
