// Charts built from GET /admin/stats/*. Each returns null when the server has no stats
// endpoint (older versions answer 404) or nothing to show, so pages simply leave them out.

import { call } from '../api.js';
import { h, bytes } from '../ui.js';
import { chart, fmt } from '../charts.js';

const SLOTS = ['--series-1', '--series-2', '--series-3', '--series-4'];
const tz = () => Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC';

async function fetchStats(route, query) {
  try {
    return await call(route, { query: { ...query, tz: tz() } });
  } catch (error) {
    if (error.code === 'not_found' || error.code === 'forbidden' || error.status === 404) return null;
    throw error;
  }
}

const dayLong = new Intl.DateTimeFormat(undefined, { weekday: 'short', day: 'numeric', month: 'short' });
const dayShort = new Intl.DateTimeFormat(undefined, { day: 'numeric', month: 'short' });
const hourFmt = new Intl.DateTimeFormat(undefined, { hour: 'numeric' });
const hourLong = new Intl.DateTimeFormat(undefined, { weekday: 'short', hour: 'numeric', minute: '2-digit' });
const asDay = (d) => new Date(`${d}T12:00:00`);

const dayTick = (label, i, n, narrow) => (i === 0 || i === n - 1 || (!narrow && i === Math.floor(n / 2)) ? dayShort.format(asDay(label)) : '');
/** Words for screen readers about days with no data; on screen a gap shows it. */
const gapText = (labels, series, long) => {
  const gaps = labels.filter((_, i) => series.every((sr) => sr.values[i] === null || sr.values[i] === undefined));
  if (!gaps.length) return '';
  return `. No data for ${gaps.slice(0, 3).map(long).join(', ')}${gaps.length > 3 ? ` and ${gaps.length - 3} more` : ''}`;
};

/** Plays per day for the last 30 days, stacked by person; four people at most, the rest as Other. */
export async function listeningChart() {
  const data = await fetchStats('GET /admin/stats/listening', { days: 30 });
  if (!data?.days?.length) return null;
  const ranked = [...data.users].map((u) => ({ ...u, total: u.plays.reduce((a, v) => a + (v ?? 0), 0) }))
    .filter((u) => u.total > 0).sort((a, b) => b.total - a.total);
  if (!ranked.length) return null;
  const top = ranked.slice(0, ranked.length > 4 ? 3 : 4);
  const rest = ranked.slice(top.length);
  const series = top.map((u, i) => ({ key: u.id, name: u.display_name || u.username, color: SLOTS[i], values: u.plays }));
  if (rest.length) {
    series.push({ key: 'other', name: `${rest.length} others`, color: '--series-other', values: data.days.map((_, i) => {
      const vals = rest.map((u) => u.plays[i]);
      return vals.every((v) => v === null) ? null : vals.reduce((a, v) => a + (v ?? 0), 0);
    }) });
  }
  const total = ranked.reduce((a, u) => a + u.total, 0);
  const long = (d) => dayLong.format(asDay(d));
  return chart({
    kind: 'bars', labels: data.days, series, toggle: series.length > 1,
    tick: dayTick, long, format: fmt.count, columnLabel: 'Day',
    ariaLabel: `Plays per day for the last 30 days, ${fmt.count(total)} in total${gapText(data.days, series, long)}`,
    figure: h('div.chart-figure', fmt.count(total), h('small', 'plays')),
  });
}

/** Streams and conversions per hour for the last 24 hours, with the conversion limit, and failures. */
export async function streamsChart() {
  const data = await fetchStats('GET /admin/stats/streams', { hours: 24 });
  if (!data?.hours?.length) return null;
  const series = [
    { key: 'streams', name: 'Playing', color: '--series-1', values: data.streams },
    { key: 'transcodes', name: 'Converting', color: '--series-2', values: data.transcodes },
  ];
  const failedTotal = (data.failed || []).reduce((a, v) => a + (v ?? 0), 0);
  if (failedTotal) series.push({ key: 'failed', name: 'Failed', color: '--status-bad', values: data.failed });
  const peak = Math.max(0, ...data.streams.map((v) => v ?? 0));
  const long = (iso) => hourLong.format(new Date(iso));
  const worst = failedTotal ? data.hours[data.failed.indexOf(Math.max(...data.failed.map((v) => v ?? 0)))] : null;
  return chart({
    kind: 'lines', labels: data.hours, series,
    tick: (label, i, n, narrow) => (i % (narrow ? 8 : 6) === 0 || i === n - 1 ? hourFmt.format(new Date(label)) : ''),
    long, format: fmt.count, columnLabel: 'Hour',
    limit: data.transcode_limit ? { value: data.transcode_limit, label: 'Limit' } : null,
    axisFormat: (t) => (Number.isInteger(t) ? String(t) : ''), minMax: 4,
    ariaLabel: `Playback over the last 24 hours, at most ${peak} at once${failedTotal ? `, ${failedTotal} failed to start, most around ${hourFmt.format(new Date(worst))}` : ''}${gapText(data.hours, series, long)}`,
    figure: h('div.chart-figure', String(peak), h('small', 'peak')),
  });
}

/**
 * Free space on the data volume over 90 days. A projection is drawn only when the last 30 days
 * fall in a steady straight line (r² ≥ 0.8) and would reach zero within three years.
 */
export async function storageChart() {
  const data = await fetchStats('GET /admin/stats/storage', { days: 90 });
  if (!data?.days?.length) return null;
  const free = data.data_free_bytes;
  const series = [{ key: 'free', name: 'Free space', color: '--series-1', values: free }];
  const fit = trend(free.slice(-30));
  let project = null;
  let projection = '';
  const last = free[free.length - 1];
  if (fit && fit.slope < 0 && fit.r2 >= 0.8 && last !== null) {
    const daysLeft = last / -fit.slope;
    if (daysLeft < 365 * 3) {
      project = { values: Array.from({ length: 30 }, (_, j) => last + fit.slope * (j + 1)) };
      const when = new Date(asDay(data.days[data.days.length - 1]).getTime() + daysLeft * 86400000);
      projection = `. Shrinking by about ${bytes(-fit.slope * 30)} a month; at this rate full around ${new Intl.DateTimeFormat(undefined, { month: 'long', year: 'numeric' }).format(when)}. The dashed line is the projection`;
    }
  }
  const long = (d) => dayLong.format(asDay(d));
  return chart({
    kind: 'lines', labels: data.days, series, project,
    tick: dayTick, long, format: bytes, axisFormat: (t) => (t === 0 ? '0' : bytes(t)), columnLabel: 'Day',
    ariaLabel: `Free space over 90 days, ${bytes(last)} now${projection}${gapText(data.days, series, long)}`,
    figure: h('div.chart-figure', bytes(last), h('small', 'free')), height: 120,
  });
}

/** Requests per day by outcome, for the Requests page. */
export async function requestsChart() {
  const data = await fetchStats('GET /admin/stats/requests', { days: 30 });
  if (!data?.days?.length) return null;
  const added = data.added.reduce((a, v) => a + (v ?? 0), 0);
  const failed = data.failed.reduce((a, v) => a + (v ?? 0), 0);
  if (!added && !failed) return null;
  const series = [
    { key: 'added', name: 'Added', color: '--status-good', values: data.added },
    { key: 'failed', name: 'Failed', color: '--status-bad', values: data.failed },
  ];
  const long = (d) => dayLong.format(asDay(d));
  return chart({
    kind: 'bars', labels: data.days, series, tick: dayTick, long, format: fmt.count, columnLabel: 'Day',
    ariaLabel: `Requests over 30 days: ${added} added, ${failed} failed${gapText(data.days, series, long)}`,
    figure: h('div.chart-figure', fmt.count(added), h('small', `added, ${fmt.count(failed)} failed`)),
  });
}

function trend(values) {
  const pts = values.map((v, i) => [i, v]).filter(([, v]) => v !== null && v !== undefined);
  if (pts.length < 14) return null;
  const n = pts.length;
  const mx = pts.reduce((a, [x]) => a + x, 0) / n;
  const my = pts.reduce((a, [, y]) => a + y, 0) / n;
  let sxy = 0; let sxx = 0; let syy = 0;
  for (const [x, y] of pts) { sxy += (x - mx) * (y - my); sxx += (x - mx) ** 2; syy += (y - my) ** 2; }
  if (!sxx || !syy) return null;
  return { slope: sxy / sxx, r2: (sxy * sxy) / (sxx * syy) };
}
