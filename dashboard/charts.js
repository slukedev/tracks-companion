// Small hand-written SVG charts: stacked bars and lines. Every value is also in a table view,
// every point is reachable by keyboard (one tab stop, arrow keys between points), and all text
// goes in through text nodes.

import { h, clean, icon } from './ui.js';

// The SVG namespace comes from an <svg> the HTML parser created, so no URL string lives in code.
let NS = null;
function svgNS() {
  if (!NS) NS = document.getElementById('svg-ns').namespaceURI;
  return NS;
}
function s(tag, attrs = {}, ...kids) {
  const el = document.createElementNS(svgNS(), tag);
  for (const [k, v] of Object.entries(attrs)) if (v !== undefined && v !== null && v !== false) el.setAttribute(k, String(v));
  for (const kid of kids) if (kid) el.append(kid instanceof Node ? kid : document.createTextNode(clean(kid)));
  return el;
}

const nf = new Intl.NumberFormat();
const compact = new Intl.NumberFormat(undefined, { notation: 'compact', maximumFractionDigits: 1 });

function niceMax(max) {
  if (max <= 0) return 4;
  const pow = 10 ** Math.floor(Math.log10(max));
  for (const step of [1, 1.2, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10]) if (step * pow >= max * 1.05) return step * pow;
  return 10 * pow;
}

/**
 * opts: {
 *   kind: 'bars' | 'lines', labels: [string] (x keys), tick: (label, i) => short text or '',
 *   long: (label) => full text for tooltips and the table,
 *   series: [{ key, name, color (css var), values: [number|null], dash? }],
 *   format: (n) => string, unit: string for aria, height?, limit?: { value, label },
 *   figure?: node, note?: node, ariaLabel: string, toggle?: boolean
 * }
 */
export function chart(opts) {
  const root = h('div.chart', { role: 'figure', 'aria-label': opts.ariaLabel });
  const head = opts.figure || opts.title || opts.controls
    ? h('div.chart-head', opts.title ? h('h3.small', opts.title) : null, opts.figure || null, opts.controls || null)
    : null;
  const plot = h('div.plot');
  const tip = h('div.chart-tip', { 'aria-hidden': 'true' });
  const live = h('p.visually-hidden', { 'aria-live': 'polite' });
  const hidden = new Set();
  const legend = opts.series.length > 1 ? h('div.chart-legend') : null;
  root.append(...[head, plot, tip, live, legend, tableView(opts)].filter(Boolean));

  const draw = () => {
    const width = Math.max(240, Math.round(plot.clientWidth || root.clientWidth || 600));
    plot.replaceChildren(render(opts, width, hidden, tip, live, root));
  };
  if (legend) {
    for (const sr of opts.series) {
      const swatch = h(`i${opts.kind === 'lines' ? '.line' : ''}`);
      swatch.style.setProperty('background', `var(${sr.color})`);
      const item = opts.toggle
        ? h('button', { type: 'button', 'aria-pressed': 'true', on: { click: () => {
          if (hidden.has(sr.key)) hidden.delete(sr.key); else if (hidden.size < opts.series.length - 1) hidden.add(sr.key);
          item.setAttribute('aria-pressed', String(!hidden.has(sr.key)));
          draw();
        } } }, swatch, sr.name)
        : h('span', swatch, sr.name);
      legend.append(item);
    }
    if (opts.limit) legend.append(h('span', h('i.dash'), opts.limit.label));
  }
  let first = true;
  const ro = new ResizeObserver(() => {
    if (!first && !root.isConnected) { ro.disconnect(); return; }
    requestAnimationFrame(() => {
      draw();
      if (first) { root.classList.add('loaded'); first = false; }
    });
  });
  ro.observe(plot);
  return root;
}

function render(opts, width, hidden, tip, live, root) {
  const narrow = width < 420;
  const height = opts.height || (narrow ? 150 : 170);
  const axisText = (t) => (opts.axisFormat ? opts.axisFormat(t) : compact.format(t));
  const n = opts.labels.length;
  const shown = opts.series.filter((sr) => !hidden.has(sr.key));

  const totals = opts.labels.map((_, i) => (opts.kind === 'bars'
    ? shown.reduce((a, sr) => a + (sr.values[i] ?? 0), 0)
    : Math.max(0, ...shown.map((sr) => sr.values[i] ?? 0))));
  const extra = opts.project ? opts.project.values.filter((v) => v !== null) : [];
  const max = niceMax(Math.max(...totals, opts.limit?.value ?? 0, ...extra, opts.minMax ?? 0));
  // The left gutter fits the widest axis label (11px tabular figures are about 6.4px a character).
  const labelWidth = Math.max(...[0, max / 2, max].map((t) => axisText(t).length)) * 6.4;
  const pad = { l: Math.max(28, Math.ceil(labelWidth) + 10), r: opts.limit ? (narrow ? 8 : 10) : 8, t: 8, b: 20 };
  const iw = width - pad.l - pad.r;
  const ih = height - pad.t - pad.b;
  const y = (v) => pad.t + ih - (v / max) * ih;
  const band = iw / (opts.project ? n + opts.project.values.length : n);
  const x = (i) => pad.l + band * i + band / 2;

  const svg = s('svg', { viewBox: `0 0 ${width} ${height}`, width, height, focusable: 'false' });
  const ticks = [0, max / 2, max];
  for (const t of ticks) {
    svg.append(s('line', { class: t === 0 ? 'axis-line' : 'grid-line', x1: pad.l, x2: width - pad.r, y1: y(t), y2: y(t) }));
    svg.append(s('text', { x: pad.l - 6, y: y(t) + 3, 'text-anchor': 'end' }, axisText(t)));
  }
  opts.labels.forEach((label, i) => {
    const text = opts.tick(label, i, n, narrow);
    if (text) svg.append(s('text', { x: x(i), y: height - 5, 'text-anchor': i === 0 ? 'start' : i === n - 1 ? 'end' : 'middle' }, text));
  });

  if (opts.kind === 'bars') {
    const bw = Math.max(2, Math.min(18, band * 0.68));
    opts.labels.forEach((_, i) => {
      let base = 0;
      const stack = shown.filter((sr) => sr.values[i]);
      stack.forEach((sr, k) => {
        const v = sr.values[i];
        const top = y(base + v);
        const bottom = y(base);
        const hgt = Math.max(0, bottom - top - (k > 0 ? 2 : 0));
        const r = k === stack.length - 1 ? Math.min(2, hgt) : 0;
        const x0 = x(i) - bw / 2;
        const d = `M${x0},${top + hgt}V${top + r}q0,-${r} ${r},-${r}h${bw - 2 * r}q${r},0 ${r},${r}V${top + hgt}Z`;
        const p = s('path', { d });
        p.style.setProperty('fill', `var(${sr.color})`);
        svg.append(p);
        base += v;
      });
      if (opts.labels.length && opts.series.every((sr) => sr.values[i] === null || sr.values[i] === undefined)) {
        svg.append(s('circle', { class: 'gap-mark', cx: x(i), cy: y(0) - 4, r: 1.5 }));
      }
    });
  } else {
    for (const sr of shown) {
      let d = '';
      let pen = false;
      sr.values.forEach((v, i) => {
        if (v === null || v === undefined) { pen = false; return; }
        d += `${pen ? 'L' : 'M'}${x(i).toFixed(1)},${y(v).toFixed(1)}`;
        pen = true;
      });
      const path = s('path', { d, fill: 'none', 'stroke-width': 2, 'stroke-linejoin': 'round', 'stroke-linecap': 'round', class: sr.dash ? 'proj-line' : '' });
      path.style.setProperty('stroke', `var(${sr.color})`);
      svg.append(path);
    }
    if (opts.project) {
      const last = opts.labels.length - 1;
      const v0 = opts.series[0].values[last];
      let d = `M${x(last)},${y(v0)}`;
      opts.project.values.forEach((v, j) => { d += `L${x(last + j + 1)},${y(Math.max(0, v))}`; });
      const p = s('path', { d, fill: 'none', 'stroke-width': 2, class: 'proj-line' });
      p.style.setProperty('stroke', `var(${opts.series[0].color})`);
      svg.append(p);
    }
  }
  if (opts.limit) {
    svg.append(s('line', { class: 'limit-line', x1: pad.l, x2: width - pad.r, y1: y(opts.limit.value), y2: y(opts.limit.value) }));
  }

  // One focus stop for the whole chart; arrow keys move between points, Home and End jump.
  const crosshair = s('line', { class: 'crosshair', y1: pad.t, y2: pad.t + ih, visibility: 'hidden' });
  const bandMark = s('rect', { class: 'hover-band', y: pad.t, height: ih, width: band, visibility: 'hidden' });
  svg.append(opts.kind === 'bars' ? bandMark : crosshair);
  const hits = s('g', { role: 'list', 'aria-label': opts.ariaLabel });
  let tipShown = false;
  const show = (i, fromKeyboard) => {
    const label = opts.long(opts.labels[i]);
    const rows = shown.map((sr) => [sr, sr.values[i]]);
    const lines = rows.map(([sr, v]) => {
      const key = h('i');
      key.style.setProperty('background', `var(${sr.color})`);
      return h('div.r', key, h('b', v === null || v === undefined ? 'no data' : opts.format(v)), h('span', sr.name));
    });
    if (opts.limit) lines.push(h('div.r.muted', h('span', `${opts.limit.label}: ${opts.format(opts.limit.value)}`)));
    tip.replaceChildren(h('div.t', label), ...lines);
    const px = x(i);
    const plotLeft = root.querySelector('.plot')?.offsetLeft || 0;
    const tipWidth = tip.offsetWidth || 170;
    const leftSide = px > width * 0.55;
    const at = leftSide ? px - tipWidth - 12 : px + 12;
    tip.style.setProperty('left', `${plotLeft + Math.min(Math.max(0, at), Math.max(0, width - tipWidth))}px`);
    tip.style.setProperty('top', `${(root.querySelector('.plot')?.offsetTop || 0) + pad.t}px`);
    tip.classList.toggle('instant', tipShown);
    tip.classList.add('on');
    tipShown = true;
    if (opts.kind === 'bars') { bandMark.setAttribute('x', String(px - band / 2)); bandMark.setAttribute('visibility', 'visible'); }
    else { crosshair.setAttribute('x1', String(px)); crosshair.setAttribute('x2', String(px)); crosshair.setAttribute('visibility', 'visible'); }
    if (fromKeyboard) live.textContent = describePoint(opts, shown, i);
  };
  const hide = () => {
    tip.classList.remove('on');
    tipShown = false;
    bandMark.setAttribute('visibility', 'hidden');
    crosshair.setAttribute('visibility', 'hidden');
  };
  const rects = opts.labels.map((_, i) => {
    const r = s('rect', {
      class: 'hit', x: pad.l + band * i, y: pad.t, width: band, height: ih,
      tabindex: i === n - 1 ? 0 : -1, role: 'listitem', 'aria-label': describePoint(opts, shown, i),
    });
    r.addEventListener('pointerenter', () => show(i, false));
    r.addEventListener('focus', () => show(i, true));
    r.addEventListener('blur', hide);
    r.addEventListener('keydown', (e) => {
      const to = { ArrowLeft: i - 1, ArrowRight: i + 1, Home: 0, End: n - 1 }[e.key];
      if (to === undefined) { if (e.key === 'Escape') { hide(); r.blur(); } return; }
      e.preventDefault();
      const next = rects[Math.max(0, Math.min(n - 1, to))];
      rects.forEach((el) => el.setAttribute('tabindex', '-1'));
      next.setAttribute('tabindex', '0');
      next.focus();
    });
    hits.append(r);
    return r;
  });
  svg.addEventListener('pointerleave', hide);
  svg.append(hits);
  svg.setAttribute('role', 'group');
  svg.setAttribute('aria-label', `${opts.ariaLabel}. Use the arrow keys to read each point.`);
  return svg;
}

function describePoint(opts, shown, i) {
  const parts = shown.map((sr) => `${sr.name} ${sr.values[i] === null || sr.values[i] === undefined ? 'no data' : opts.format(sr.values[i])}`);
  return `${opts.long(opts.labels[i])}: ${parts.join(', ')}`;
}

function tableView(opts) {
  const cols = opts.series.map((sr) => sr.name);
  const rows = opts.labels.map((label, i) => h('tr',
    h('th', { scope: 'row' }, opts.long(label)),
    opts.series.map((sr) => h('td', sr.values[i] === null || sr.values[i] === undefined ? '' : opts.format(sr.values[i])))));
  return h('details',
    h('summary', { title: 'Show as a table' }, icon('table'), h('span.visually-hidden', `Show ${opts.name || 'the chart'} as a table`)),
    h('div.table-scroll', { tabindex: '0', role: 'region', 'aria-label': `${opts.ariaLabel}, table` },
      h('table',
        h('thead', h('tr', h('th', { scope: 'col' }, opts.columnLabel || 'When'), cols.map((c) => h('th', { scope: 'col' }, c)))),
        h('tbody', [...rows].reverse()))));
}

export const fmt = { count: (n) => nf.format(n) };
