import { call } from '../api.js';
import { h, pageHead, list, button, busy, empty, time, dayLabel, select, nextId, failed, icon } from '../ui.js';
import { sentence } from './history.js';

const ACTIONS = [
  ['', 'Everything'],
  ['user', 'People'],
  ['invite', 'Invites'],
  ['device', 'Devices'],
  ['pairing', 'Pairing'],
  ['key', 'Keys'],
  ['library', 'Libraries'],
  ['backup', 'Backups'],
  ['settings', 'Settings'],
];

export async function render(ctx) {
  const [{ users }, firstPage] = await Promise.all([
    call('GET /admin/users').catch(() => ({ users: [] })),
    call('GET /admin/audit', { query: { limit: 50 } }),
  ]);
  const names = new Map(users.map((u) => [u.username, u.display_name || u.username]));
  const actions = ctx.acquisition ? [...ACTIONS, ['acqui', 'Requests']] : ACTIONS;

  const actionId = nextId('a');
  const actorId = nextId('a');
  const action = select(actions, '', { id: actionId });
  const actor = select([['', 'Anyone'], ['system', 'Trackside'], ...users.map((u) => [u.username, u.display_name || u.username])], '', { id: actorId });
  const results = h('div.history.history-days', { 'aria-live': 'polite' });
  const more = button('Show More', {});
  let cursor = firstPage.next_cursor;
  let lastDay = '';
  let currentList = null;

  const add = (entries) => {
    for (const entry of entries.filter((e) => ctx.acquisition || !/^acqui/.test(e.action))) {
      const day = dayLabel(entry.at);
      if (day !== lastDay) {
        lastDay = day;
        currentList = list([], { iconInset: true });
        results.append(h('div.day', h('h2.small.muted', day), currentList));
      }
      const s = sentence(entry, names);
      const who = entry.ip ? `from ${entry.ip}` : null;
      currentList.append(h('div.row',
        icon(s.icon, `tint-${s.tint}`),
        h('div.row-main', h('div.row-title.history-text', s.text), who ? h('div.row-sub', who) : null),
        h('div.row-trail.num.small', h('time', { datetime: entry.at }, time(entry.at)))));
    }
    more.hidden = !cursor;
  };

  const load = async (reset) => {
    const query = { limit: 50, action: action.value, actor: actor.value };
    if (!reset) query.cursor = cursor;
    try {
      const page = await call('GET /admin/audit', { query });
      if (reset) { results.replaceChildren(); lastDay = ''; }
      cursor = page.next_cursor;
      if (reset && !page.entries.length) results.append(list(empty('Nothing matches', 'Try a different filter.')));
      add(page.entries);
    } catch (err) { failed(err); }
  };
  action.addEventListener('change', () => load(true));
  actor.addEventListener('change', () => load(true));
  more.addEventListener('click', () => busy(more, () => load(false)));

  if (firstPage.entries.length) add(firstPage.entries);
  else results.append(list(empty('Nothing yet', 'Every change made on this server is listed here.')));
  more.hidden = !cursor;

  return h('div.page.narrow',
    pageHead('Activity'),
    h('div.toolbar',
      h('label.visually-hidden', { for: actionId }, 'Show'), action,
      h('label.visually-hidden', { for: actorId }, 'By'), actor),
    results,
    h('div.btn-row', more));
}
