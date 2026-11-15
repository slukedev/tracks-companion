// Requests: songs people asked the server to find, and the sources it finds them with.
// This is the only page that names the sources (yt-dlp, Soulseek through slskd).

import { call, describe } from '../api.js';
import { requestsChart } from './stats.js';
import {
  h, pageHead, section, list, badge, button, busy, field, input, select, toggle, segmented, nextId, empty,
  ago, agoInline, when, bytes, plural, confirm, toast, failed, formError, meter, icon,
} from '../ui.js';

const PROVIDERS = {
  ytdlp: { title: 'yt-dlp', about: 'Finds songs on YouTube Music and YouTube.' },
  soulseek: { title: 'Soulseek', about: 'Finds files shared by other people, through slskd.' },
};
const providerTitle = (name) => PROVIDERS[name]?.title || name;

const HEALTH = {
  ok: ['Working', 'good'],
  degraded: ['Struggling', 'warn'],
  cooldown: ['Paused', 'warn'],
  outdated: ['Out of date', 'warn'],
  unavailable: ['Not reachable', 'bad'],
  disabled: ['Off', ''],
};

const STATE = {
  queued: ['Waiting', ''],
  searching: ['Searching', 'accent'],
  downloading: ['Downloading', 'accent'],
  verifying: ['Checking', 'accent'],
  importing: ['Adding', 'accent'],
  done: ['Added', 'good'],
  failed: ['Failed', 'bad'],
  cancelled: ['Cancelled', ''],
};

const FAILURE = {
  no_match: 'No match',
  all_candidates_failed: 'Every match failed',
  exhausted: 'Gave up',
  provider_unavailable: 'No source reachable',
  insufficient_space: 'Disk full',
  rejected_file: 'File rejected',
  quota_exceeded: 'Daily limit reached',
  disabled: 'Requests turned off',
};

const ACTIVE = ['queued', 'searching', 'downloading', 'verifying', 'importing'];

export async function render(ctx) {
  const [{ providers }, settings] = await Promise.all([
    call('GET /admin/acquire/providers'),
    call('GET /admin/settings').catch(() => ({})),
  ]);
  const filter = { state: ctx.args[0] || 'active' };
  const requestsSlot = h('div');
  const trendSlot = h('div', { hidden: true });
  requestsChart().then((node) => {
    if (node && ctx.isCurrent()) trendSlot.replaceWith(section('Last 30 Days', h('div.list', node)));
  }).catch(() => {});
  const page = h('div.page',
    pageHead('Requests'),
    trendSlot,
    requestsBlock(ctx, filter, requestsSlot),
    sourcesSection(ctx, providers),
    rulesSection(ctx, settings));
  await loadRequests(ctx, filter, requestsSlot);
  return page;
}

/* ---------- Requests ---------- */

function requestsBlock(ctx, filter, slot) {
  const filters = segmented('req-state', [['active', 'In Progress'], ['failed', 'Failed'], ['finished', 'Finished'], ['all', 'All']], filter.state, (v) => {
    filter.state = v;
    history.replaceState(null, '', `#/requests/${v}`);
    loadRequests(ctx, filter, slot);
  }, 'Show');
  const id = nextId('s');
  return h('section.section', { 'aria-labelledby': id },
    h('div.section-head', h('h2', { id }, 'Songs'), filters),
    slot);
}

async function loadRequests(ctx, filter, slot) {
  const query = { limit: 50 };
  if (filter.state === 'active' || filter.state === 'finished') query.state = filter.state;
  let items;
  try {
    ({ acquisitions: items } = await call('GET /admin/acquisitions', { query }));
  } catch (err) {
    slot.replaceChildren(h('div.error-state', h('p', describe(err))));
    return;
  }
  if (filter.state === 'failed') items = items.filter((a) => a.state === 'failed');
  else if (filter.state === 'active') items = items.filter((a) => ACTIVE.includes(a.state));
  else if (filter.state === 'finished') items = items.filter((a) => !ACTIVE.includes(a.state));
  const text = { active: 'Nothing in progress', failed: 'Nothing has failed', finished: 'Nothing finished yet', all: 'No requests yet' }[filter.state];
  slot.replaceChildren(list(items.length ? items.map((a) => requestRow(ctx, a, filter, slot)) : [empty(text, filter.state === 'all' ? 'Songs people ask for in Tracks appear here.' : null)], { iconInset: true }));
}

function requestRow(ctx, a, filter, slot) {
  const [label, kind] = STATE[a.state] || [a.state, ''];
  const detail = h('div.detail', { hidden: true });
  const more = button('Details', { kind: 'plain' });
  more.setAttribute('aria-expanded', 'false');
  more.setAttribute('aria-label', `Details for ${a.title}`);
  more.addEventListener('click', () => {
    const open = more.getAttribute('aria-expanded') === 'true';
    more.setAttribute('aria-expanded', String(!open));
    detail.hidden = open;
    if (!open && !detail.childElementCount) detail.append(...requestDetail(ctx, a, () => loadRequests(ctx, filter, slot)));
  });

  const who = a.user?.display_name || a.user?.username;
  const sub = [a.state === 'failed' && a.failure ? FAILURE[a.failure] || a.failure : null, who, agoInline(a.updated_at)].filter(Boolean).join(', ');
  const progress = a.state === 'downloading' && a.progress
    ? h('div.form.tight', meter(a.progress.bytes, a.progress.total, `Download of ${a.title}`),
      h('div.row-sub.num', a.progress.total ? `${bytes(a.progress.bytes)} / ${bytes(a.progress.total)}` : bytes(a.progress.bytes)))
    : null;
  return h('div',
    h('div.row',
      icon(a.state === 'failed' ? 'warning' : a.state === 'done' ? 'check' : 'requests', a.state === 'failed' ? 'tint-bad' : a.state === 'done' ? 'tint-good' : 'tint-accent'),
      h('div.row-main',
        h('div.row-title', `${a.title}, ${a.artist}`, badge(label, kind)),
        h('div.row-sub', sub),
        progress),
      h('div.row-trail', more)),
    detail);
}

function requestDetail(ctx, a, reload) {
  const parts = [];
  if (a.candidate) {
    parts.push(h('div.small',
      h('div', h('strong', 'Source: '), `${providerTitle(a.candidate.provider)}, ${a.candidate.source}`),
      a.candidate.format ? h('div.muted', [a.candidate.format.codec?.toUpperCase(), a.candidate.format.bitrate_kbps ? `${a.candidate.format.bitrate_kbps} kbps` : null, a.candidate.size ? bytes(a.candidate.size) : null].filter(Boolean).join(', ')) : null));
  }
  if (a.attempts?.length) {
    parts.push(h('div.small.form.tight',
      h('strong', 'Tried'),
      a.attempts.map((t) => h('div', h('div', `${providerTitle(t.provider)}, ${t.source}`), h('div.muted', t.error || t.failure || '')))));
  }
  const actions = h('div.btn-row');
  if (a.state === 'failed' || a.state === 'cancelled') {
    const retry = button('Try Again', { iconName: 'scan' });
    retry.addEventListener('click', () => busy(retry, async () => {
      try {
        await call('POST /admin/acquisitions/{id}/retry', { values: { id: a.id } });
        toast('Trying again');
        reload();
      } catch (err) { failed(err); }
    }));
    actions.append(retry);
  }
  if (ACTIVE.includes(a.state)) {
    const cancel = button('Cancel Request', { kind: 'danger' });
    cancel.addEventListener('click', async () => {
      if (!await confirm({ title: `Cancel ${a.title}?`, text: `${a.user?.display_name || 'They'} will see it as cancelled.`, action: 'Cancel Request' })) return;
      await busy(cancel, async () => {
        try {
          await call('DELETE /admin/acquisitions/{id}', { values: { id: a.id } });
          toast('Cancelled');
          reload();
        } catch (err) { failed(err); }
      });
    });
    actions.append(cancel);
  }
  if (!parts.length && !actions.childElementCount) parts.push(h('p.small.muted', 'No source chosen yet.'));
  if (actions.childElementCount) parts.push(actions);
  return parts;
}

/* ---------- Sources ---------- */

function sourcesSection(ctx, providers) {
  return section('Sources', h('div.form', providers.map((p) => providerPanel(ctx, p))),
    {});
}

function providerPanel(ctx, p) {
  const meta = PROVIDERS[p.name] || { title: p.name, about: '' };
  const [label, kind] = HEALTH[p.health] || [p.health, ''];
  const id = nextId('p');
  const enabled = toggle(p.enabled, { 'aria-labelledby': id });
  enabled.addEventListener('change', async () => {
    try {
      await call('PATCH /admin/acquire/providers/{name}', { values: { name: p.name }, body: { enabled: enabled.checked } });
      toast(`${meta.title} ${enabled.checked ? 'on' : 'off'}`);
      if (ctx.isCurrent()) ctx.rerender();
    } catch (err) { enabled.checked = !enabled.checked; failed(err); }
  });

  const newer = p.latest_known && p.version && p.latest_known !== p.version;
  const status = [];
  if (p.health === 'cooldown' && p.cooldown_until) status.push(`Resumes ${when(p.cooldown_until)}`);
  else if (p.health !== 'ok' && p.detail) status.push(p.detail);

  const concurrency = select([1, 2, 3, 4, 6, 8].map((n) => [String(n), String(n)]), String(p.settings?.concurrency ?? 2), { id: nextId('c') });
  const fields = [field('Downloads at once', concurrency)];
  let url;
  let apiKey;
  if (p.name === 'soulseek') {
    url = input({ value: p.settings?.url || '', mono: true, spellcheck: 'false', placeholder: 'slskd:5030' });
    apiKey = input({ type: 'password', autocomplete: 'off', placeholder: 'Unchanged', mono: true });
    fields.push(field('slskd address', url), field('slskd API key', apiKey));
  }
  const error = formError();
  const save = button('Save', { type: 'submit' });
  const form = h('form.form', {
    on: {
      submit: async (event) => {
        event.preventDefault();
        error.hide();
        const body = { concurrency: Number(concurrency.value) };
        if (url) body.url = url.value.trim();
        if (apiKey?.value) body.api_key = apiKey.value;
        await busy(save, async () => {
          try {
            await call('PATCH /admin/acquire/providers/{name}', { values: { name: p.name }, body });
            if (apiKey) apiKey.value = '';
            toast('Saved');
          } catch (err) { error.show(describe(err)); }
        });
      },
    },
  }, h('div.fields-2', fields, h('div.field.save-cell', h('span.label', { 'aria-hidden': 'true' }, ' '), save)), error.el);

  const testQuery = input({ value: 'beyonce halo', 'aria-label': `Test search for ${meta.title}` });
  const testResult = h('p.small', { role: 'status' });
  const test = button('Test Search', {});
  test.addEventListener('click', () => busy(test, async () => {
    testResult.textContent = '';
    try {
      const r = await call('POST /admin/acquire/providers/{name}/test', { values: { name: p.name }, body: { query: testQuery.value.trim() || 'test' } });
      testResult.textContent = r.ok === false
        ? `Failed: ${r.detail || 'no answer'}`
        : `${plural(r.candidates ?? 0, 'result')} in ${((r.elapsed_ms ?? 0) / 1000).toFixed(1)} s.`;
    } catch (err) { testResult.textContent = describe(err); }
  }));

  let update = null;
  if (p.name === 'ytdlp') {
    update = button(newer ? `Update to ${p.latest_known}` : 'Check for Update', { kind: newer ? 'primary' : '' });
    update.addEventListener('click', () => busy(update, async () => {
      try {
        await call('POST /admin/acquire/providers/ytdlp/update');
        toast('Updating yt-dlp');
        setTimeout(() => { if (ctx.isCurrent()) ctx.rerender(); }, 4000);
      } catch (err) { failed(err); }
    }));
  }


  return h('div.panel',
    h('div.setting-row.flush',
      h('div.row-main',
        h('div.row-title', h('span', { id, title: meta.about }, meta.title), badge(label, kind)),
        status.length ? h('div.row-sub', status.join('. ')) : null),
      h('div.control', enabled)),
    update ? h('div.btn-row', update) : null,
    form,
    h('div.form.tight',
      h('div.btn-row.nowrap-row', testQuery, test),
      testResult));
}

/* ---------- Rules ---------- */

function rulesSection(ctx, settings) {
  const value = (key, fallback) => settings[`acquire.${key}`]?.value ?? fallback;
  const locked = (key) => settings[`acquire.${key}`] && !settings[`acquire.${key}`].editable;
  const on = toggle(value('enabled', false), { disabled: locked('enabled'), 'aria-label': 'Requests' });
  const instant = toggle(value('instant_play', false), { disabled: locked('instant_play'), 'aria-label': 'Play while downloading' });
  const memberDaily = input({ type: 'number', min: '0', value: String(value('member_daily', 50)), disabled: locked('member_daily') });
  const memberActive = input({ type: 'number', min: '1', value: String(value('member_active', 5)), disabled: locked('member_active') });
  const globalDaily = input({ type: 'number', min: '0', value: String(value('global_daily', 500)), disabled: locked('global_daily') });
  [memberDaily, memberActive, globalDaily].forEach((i) => i.classList.add('short'));
  const prefer = value('prefer', ['soulseek', 'ytdlp']);
  const preferSelect = select([['soulseek,ytdlp', 'Soulseek first'], ['ytdlp,soulseek', 'yt-dlp first']], prefer.join(','), { disabled: locked('prefer') });
  const tags = toggle(Boolean(settings['enrichment.musicbrainz']?.value), { disabled: settings['enrichment.musicbrainz'] && !settings['enrichment.musicbrainz'].editable, 'aria-label': 'Look up tags on MusicBrainz' });

  const patch = async (body, revert) => {
    try {
      await call('PATCH /admin/acquire', { body });
      toast('Saved');
    } catch (err) { revert?.(); failed(err); }
  };
  on.addEventListener('change', () => patch({ enabled: on.checked }, () => { on.checked = !on.checked; }));
  instant.addEventListener('change', () => patch({ instant_play: instant.checked }, () => { instant.checked = !instant.checked; }));
  for (const [el, key] of [[memberDaily, 'member_daily'], [memberActive, 'member_active'], [globalDaily, 'global_daily']]) {
    el.addEventListener('change', () => { if (el.value !== '' && Number(el.value) >= 0) patch({ [key]: Number(el.value) }); });
  }
  preferSelect.addEventListener('change', () => patch({ prefer: preferSelect.value.split(',') }));
  tags.addEventListener('change', async () => {
    try {
      await call('PATCH /admin/settings', { body: { 'enrichment.musicbrainz': tags.checked } });
      toast('Saved');
    } catch (err) { tags.checked = !tags.checked; failed(err); }
  });

  const setting = (title, sub, control) => {
    if (!control.id) control.id = nextId('r');
    return h('div.setting-row', h('div.row-main', h('label.row-title', { for: control.id }, title), sub ? h('div.row-sub', sub) : null), h('div.control', control));
  };
  return section('Rules', h('div.list',
    setting('Requests', null, on),
    setting('Instant play', null, instant),
    setting('Try first', null, preferSelect),
    setting('Per person daily', null, memberDaily),
    setting('Per person at once', null, memberActive),
    setting('Server daily', null, globalDaily),
    setting('MusicBrainz tags', null, tags)));
}

