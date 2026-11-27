// DOM helpers. Every piece of text goes in through text nodes; nothing here parses HTML.

import { describe, now } from './api.js';

/**
 * h('div.row', {attrs}, ...children). Attribute keys: `class`, `on` (event map), `dataset`,
 * boolean or string attributes; properties for value/checked/disabled. Children: nodes,
 * strings, numbers, arrays, null.
 */
export function h(spec, attrs, ...children) {
  if (attrs === null || typeof attrs !== 'object' || attrs instanceof Node || Array.isArray(attrs)) {
    children.unshift(attrs);
    attrs = {};
  }
  const [tag, ...classes] = spec.split('.');
  const el = document.createElement(tag || 'div');
  if (classes.length) el.className = classes.join(' ');
  for (const [key, value] of Object.entries(attrs)) {
    if (value === undefined || value === null || value === false) continue;
    if (key === 'class') el.className = [el.className, value].filter(Boolean).join(' ');
    else if (key === 'on') for (const [ev, fn] of Object.entries(value)) el.addEventListener(ev, fn);
    else if (key === 'dataset') Object.assign(el.dataset, value);
    else if (key === 'value' || key === 'checked' || key === 'disabled' || key === 'selected' || key === 'indeterminate') el[key] = value;
    else if (key === 'style') throw new Error('inline styles are not allowed under the CSP');
    else el.setAttribute(key, value === true ? '' : clean(value));
  }
  append(el, children);
  return el;
}

// Control characters and bidirectional overrides let a name like "evil\u202Egnp.exe" display as
// something else. Names come from other people (devices, libraries, usernames), so strip them.
const UNSAFE_TEXT = /[\u0000-\u0008\u000B\u000C\u000E-\u001F\u007F-\u009F\u200E\u200F\u202A-\u202E\u2066-\u2069]/g;
export const clean = (value) => String(value).replace(UNSAFE_TEXT, '');

export function append(el, children) {
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false) continue;
    el.append(child instanceof Node ? child : document.createTextNode(clean(child)));
  }
  return el;
}

export function clear(el) {
  while (el.firstChild) el.firstChild.remove();
  return el;
}

export function icon(name, extra = '') {
  return h(`span.icon.i-${name}${extra ? '.' + extra.split(' ').join('.') : ''}`, { 'aria-hidden': 'true' });
}

let uid = 0;
export const nextId = (prefix = 'f') => `${prefix}${++uid}`;

/* ---------- Formatting ---------- */

const nf = new Intl.NumberFormat();
export const num = (n) => nf.format(n ?? 0);

export function bytes(n) {
  if (n === undefined || n === null) return '';
  const units = ['bytes', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  let v = n;
  while (v >= 1000 && i < units.length - 1) { v /= 1000; i++; }
  const digits = i === 0 ? 0 : v < 100 ? 1 : 0;
  return `${new Intl.NumberFormat(undefined, { maximumFractionDigits: digits }).format(v)} ${units[i]}`;
}

const timeFmt = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit' });
const dateFmt = new Intl.DateTimeFormat(undefined, { day: 'numeric', month: 'short' });
const dateYearFmt = new Intl.DateTimeFormat(undefined, { day: 'numeric', month: 'short', year: 'numeric' });
const dayFmt = new Intl.DateTimeFormat(undefined, { weekday: 'long', day: 'numeric', month: 'long' });
const fullFmt = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' });
const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' });

const startOfDay = (t) => { const d = new Date(t); d.setHours(0, 0, 0, 0); return d.getTime(); };

export const time = (iso) => timeFmt.format(new Date(iso));
export const full = (iso) => fullFmt.format(new Date(iso));

/** "Today", "Yesterday", or a date, for grouping. */
export function dayLabel(iso) {
  const diff = Math.round((startOfDay(now()) - startOfDay(Date.parse(iso))) / 86400000);
  if (diff === 0) return 'Today';
  if (diff === 1) return 'Yesterday';
  return dayFmt.format(new Date(iso));
}

/** "Just now", "12 min ago", "Yesterday at 21:10", "3 Oct". */
export function ago(iso) {
  if (!iso) return 'Never';
  const t = Date.parse(iso);
  const s = Math.round((now() - t) / 1000);
  if (s < 0) return when(iso);
  if (s < 60) return 'Just now';
  if (s < 3600) return rtf.format(-Math.round(s / 60), 'minute');
  const days = Math.round((startOfDay(now()) - startOfDay(t)) / 86400000);
  if (days === 0) return `Today at ${time(iso)}`;
  if (days === 1) return `Yesterday at ${time(iso)}`;
  return date(iso);
}

/** ago() for the middle of a sentence: "used yesterday at 21:10", "used 12 Nov". */
export function agoInline(iso) {
  const s = ago(iso);
  return /^(Today|Yesterday|Just|Never)/.test(s) ? s.charAt(0).toLowerCase() + s.slice(1) : s;
}

/** A future time: "in 40 min", "tomorrow at 03:30", "21 Nov". */
export function when(iso) {
  const t = Date.parse(iso);
  const s = Math.round((t - now()) / 1000);
  if (s <= 0) return ago(iso);
  if (s < 3600) return rtf.format(Math.max(1, Math.round(s / 60)), 'minute');
  const days = Math.round((startOfDay(t) - startOfDay(now())) / 86400000);
  if (days === 0) return `today at ${time(iso)}`;
  if (days === 1) return `tomorrow at ${time(iso)}`;
  return date(iso);
}

export function date(iso) {
  const d = new Date(iso);
  return (d.getFullYear() === new Date(now()).getFullYear() ? dateFmt : dateYearFmt).format(d);
}

export function duration(seconds) {
  const d = Math.floor(seconds / 86400);
  const hrs = Math.floor((seconds % 86400) / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  if (d) return `${d} ${d === 1 ? 'day' : 'days'}${hrs ? `, ${hrs} h` : ''}`;
  if (hrs) return `${hrs} h ${m} min`;
  return `${m} min`;
}

export const plural = (n, one, many = `${one}s`) => `${num(n)} ${n === 1 ? one : many}`;

/* ---------- Page scaffolding ---------- */

export function pageHead(title, { lede, actions, crumb } = {}) {
  return h('header.page-head',
    h('div',
      crumb ? h('p.crumb', h('a', { href: crumb[1] }, crumb[0])) : null,
      h('h1', { tabindex: '-1', 'data-page-title': '' }, title),
      lede ? h('p.lede', lede) : null),
    actions ? h('div.actions', actions) : null);
}

export function section(title, children, { aside, foot, id, headingLevel = 'h2' } = {}) {
  const headingId = id || nextId('s');
  return h('section.section', { 'aria-labelledby': headingId },
    h('div.section-head', h(headingLevel, { id: headingId }, title), aside ? h('div.aside', aside) : null),
    children,
    foot ? h('p.section-foot', foot) : null);
}

export function list(children, { iconInset = false, label } = {}) {
  return h(`div.list${iconInset ? '.inset-icon' : ''}`, { role: label ? 'list' : undefined, 'aria-label': label }, children);
}

export function row({ icon: iconName, tint, title, sub, trail, href: link, onClick, label, wrapTrail } = {}) {
  const body = [
    iconName ? icon(iconName, tint ? `tint-${tint}` : '') : null,
    h('div.row-main', h('div.row-title', title), sub ? h('div.row-sub', sub) : null),
    trail ? h(`div.row-trail${wrapTrail ? '.wrap' : ''}`, trail) : null,
  ];
  if (link) return h('a.row', { href: link, 'aria-label': label }, body, icon('chevron', 'sm chev'));
  if (onClick) return h('button.row', { type: 'button', on: { click: onClick }, 'aria-label': label }, body, icon('chevron', 'sm chev'));
  return h('div.row', body);
}

export function kv(key, value, sub) {
  return h('div.kv', h('dt', key), h('dd', value, sub ? h('span.sub', sub) : null));
}

export function empty(title, text, action) {
  return h('div.empty', h('h3', title), text ? h('p', text) : null, action || null);
}

export function badge(text, kind = '', iconName) {
  return h(`span.badge${kind ? '.' + kind : ''}`, iconName ? icon(iconName) : null, text);
}

const skelRows = (n, widths = ['w60', 'w40']) => Array.from({ length: n }, () => h('div.skeleton-row', widths.map((w) => h(`span.skeleton.${w}`))));

/**
 * A placeholder shaped like the page that is loading, so nothing jumps when it arrives.
 * `kind` is 'overview', 'detail' (a narrow page of sections) or 'list'.
 */
export function skeleton(kind = 'list') {
  const head = h('div.skeleton-head', h('span.skeleton.title'), h('span.skeleton.line'));
  let body;
  if (kind === 'overview') {
    body = [
      h('div.list', skelRows(2, ['w40', 'w60'])),
      h('div.overview-grid',
        h('div.col', h('div.list', h('div.skeleton-row', h('span.skeleton.w25'), h('span.skeleton.block'))), h('div.list', skelRows(4, ['w60']))),
        h('div.facts', h('div.list', skelRows(3, ['w40'])), h('div.list', skelRows(4, ['w60'])))),
    ];
  } else if (kind === 'detail') {
    body = [h('div.list', skelRows(3)), h('div.list', skelRows(2))];
  } else {
    body = [h('div.list', skelRows(5))];
  }
  return h(`div.page${kind === 'detail' ? '.narrow' : ''}`, { 'aria-busy': 'true', 'aria-label': 'Loading' }, head, body);
}

const RECOVERY = {
  network: 'Check that Trackside is running and this device is on the same network, then try again.',
  internal: 'Trying again often works. If it keeps happening, the server log says why.',
  unavailable: 'The server is busy. Try again in a moment.',
};

export function errorState(error, retry, title = 'This page did not load') {
  return h('div.error-state', { role: 'alert' },
    h('h2', title),
    h('p', describe(error), RECOVERY[error?.code] ? ` ${RECOVERY[error.code]}` : ''),
    retry ? button('Try again', { onClick: retry }) : null);
}

/* ---------- Controls ---------- */

export function button(label, { kind = '', onClick, type = 'button', iconName, disabled, title, ariaLabel, size = '' } = {}) {
  const classes = ['btn', kind, size].filter(Boolean).join('.');
  const iconOnly = !label;
  return h(`button.${classes}${iconOnly ? '.icon-only' : ''}`, {
    type, disabled, title: title || (iconOnly ? ariaLabel : undefined), 'aria-label': ariaLabel,
    on: onClick ? { click: onClick } : undefined,
  }, iconName ? icon(iconName, 'sm') : null, label);
}

/** Runs an async action with the button busy, so double clicks do nothing. */
export async function busy(btn, task) {
  if (btn.getAttribute('aria-busy') === 'true') return undefined;
  btn.setAttribute('aria-busy', 'true');
  btn.disabled = true;
  try {
    return await task();
  } finally {
    btn.removeAttribute('aria-busy');
    if (!btn.dataset.hold) btn.disabled = false;
  }
}

export function field(labelText, control, { hint, id } = {}) {
  const controlId = id || control.id || nextId('f');
  control.id = controlId;
  if ('name' in control && !control.name) control.name = controlId;
  const hintEl = hint ? h('p.hint', { id: `${controlId}-hint` }, hint) : null;
  if (hintEl) control.setAttribute('aria-describedby', hintEl.id);
  const errorEl = h('p.error', { id: `${controlId}-error`, hidden: true });
  return h('div.field', h('label', { for: controlId }, labelText), control, hintEl, errorEl);
}

export function input(attrs = {}) {
  const el = h(`input.input${attrs.mono ? '.mono' : ''}`, { type: 'text', autocomplete: 'off', ...attrs, mono: undefined });
  if (attrs.mono) el.setAttribute('translate', 'no');
  return el;
}

export function select(options, value, attrs = {}) {
  return h('select.input', attrs, options.map(([v, label]) => h('option', { value: v, selected: String(v) === String(value) }, label)));
}

export function toggle(checked, attrs = {}) {
  return h('input.switch', { type: 'checkbox', role: 'switch', checked, ...attrs });
}

export function checkbox(labelText, checked, { sub, name, value, disabled } = {}) {
  const box = h('input', { type: 'checkbox', checked, name, value, disabled });
  return h('label.check', box, h('span.row-main', h('span', labelText), sub ? h('span.row-sub', sub) : null));
}

export function segmented(name, options, value, onChange, label) {
  return h('div.segmented', { role: 'radiogroup', 'aria-label': label },
    options.map(([v, text]) => h('label',
      h('input', { type: 'radio', name, value: v, checked: v === value, on: { change: () => onChange(v) } }),
      h('span', text))));
}

export function setFieldError(control, message) {
  const err = document.getElementById(`${control.id}-error`);
  if (message) {
    control.setAttribute('aria-invalid', 'true');
    if (err) { err.textContent = message; err.hidden = false; }
  } else {
    control.removeAttribute('aria-invalid');
    if (err) { err.textContent = ''; err.hidden = true; }
  }
}

export function formError() {
  const el = h('div.form-error', { role: 'alert', hidden: true });
  return {
    el,
    show(message) { clear(el); append(el, [icon('warning'), h('span', message)]); el.hidden = false; },
    hide() { el.hidden = true; clear(el); },
  };
}

/** A read-only value with a Copy button that briefly says Copied. */
export function secret(value, label) {
  const box = h('input.input', { readonly: true, value, 'aria-label': label, on: { focus: (e) => e.target.select() } });
  return h('div.secret', box, h('div.btn-row', copyButton(value)));
}

/**
 * A link meant for someone else (an invite, a password reset): shown whole, with Share where the
 * device has a share sheet and Copy everywhere.
 */
export function shareLink(url, { label, title, text } = {}) {
  const box = h('code.link-box', { 'aria-label': label, translate: 'no' }, url);
  const actions = h('div.btn-row');
  if (typeof navigator.share === 'function') {
    const share = button('Share', { kind: 'primary', iconName: 'share' });
    share.addEventListener('click', async () => {
      try {
        await navigator.share({ title, text, url });
      } catch (err) {
        if (err?.name !== 'AbortError') toast('Could not open the share sheet. Copy the link instead.', 'bad');
      }
    });
    actions.append(share);
  }
  actions.append(copyButton(url, 'Copy link'));
  return h('div.form.tight', box, actions);
}

async function writeClipboard(value) {
  try {
    await navigator.clipboard.writeText(value);
  } catch {
    const tmp = h('textarea.visually-hidden', { readonly: true }, value);
    document.body.append(tmp);
    tmp.select();
    document.execCommand('copy');
    tmp.remove();
  }
}

/** Swaps a button's label for a moment ("Copied"), then puts it back. */
function flashLabel(btn, text, original) {
  const label = btn.lastChild;
  label.replaceWith(h('span.swap', text));
  clearTimeout(btn.flashTimer);
  btn.flashTimer = setTimeout(() => { btn.lastChild.replaceWith(h('span.swap', original)); }, 1800);
}

export function copyButton(value, text = 'Copy') {
  const btn = button(text, { iconName: 'copy' });
  btn.addEventListener('click', async () => {
    await writeClipboard(value);
    flashLabel(btn, 'Copied', text);
  });
  return btn;
}

/** A command or config line with a Copy button: the thing to type, not instructions. */
export function codeLine(value, label = 'Copy') {
  return h('div.code-line', h('code', { translate: 'no' }, value), copyButton(value, label));
}

/* ---------- Dialog and toast ---------- */

/**
 * Shows a modal dialog and removes it once its exit transition has played.
 * `onClose(returnValue)` runs as it starts closing.
 */
export function openDialog(dialog, onClose) {
  document.body.append(dialog);
  dialog.addEventListener('close', () => {
    onClose?.(dialog.returnValue);
    setTimeout(() => dialog.remove(), 220);
  }, { once: true });
  dialog.showModal();
  return dialog;
}

/** Asks once before something destructive. Resolves true when confirmed. */
export function confirm({ title, text, action = 'Delete', danger = true, extra }) {
  return new Promise((resolve) => {
    const titleId = nextId('d');
    const dialog = h('dialog', { 'aria-labelledby': titleId },
      h('form.dialog-form', { method: 'dialog' },
        h('div.dialog-body', h('h2', { id: titleId }, title), text ? h('p', text) : null, extra || null),
        h('div.dialog-actions',
          h('button.btn', { value: 'cancel', type: 'submit' }, 'Cancel'),
          h(`button.btn.${danger ? 'danger-fill' : 'primary'}`, { value: 'ok', type: 'submit' }, action))));
    openDialog(dialog, (value) => resolve(value === 'ok'));
    dialog.querySelector('button[value="cancel"]').focus();
  });
}

let toastRegion;
export function toast(message, kind = '') {
  if (!toastRegion) {
    toastRegion = h('div.toast-region', { role: 'status', 'aria-live': 'polite' });
    document.body.append(toastRegion);
  }
  const el = h(`div.toast${kind ? '.' + kind : ''}`, icon(kind === 'bad' ? 'warning' : 'check', 'sm'), message);
  toastRegion.append(el);
  setTimeout(() => {
    el.classList.add('leaving');
    setTimeout(() => el.remove(), 200);
  }, kind === 'bad' ? 5000 : 2400);
}

export function failed(error) {
  toast(describe(error), 'bad');
}

/** A meter for one ratio against a limit. */
export function meter(value, total, label, kind = '') {
  if (!(total > 0)) return null;
  const pct = Math.min(100, Math.max(0, (value / total) * 100));
  const bar = h(`div.meter${kind ? '.' + kind : ''}`, {
    role: 'progressbar', 'aria-label': label,
    'aria-valuemin': '0', 'aria-valuemax': '100', 'aria-valuenow': String(Math.round(pct)),
  }, h('span'));
  bar.firstChild.style.setProperty('--value', String(pct / 100));
  return bar;
}

export const platformName = (p) => ({ ios: 'iPhone', ipados: 'iPad', macos: 'Mac', android: 'Android', android_tv: 'Android TV', web: 'Browser' }[p] || p || 'Device');
export const platformIcon = (p) => ({ ios: 'phone', android: 'phone', android_tv: 'tv', ipados: 'tablet', macos: 'laptop', windows: 'laptop', linux: 'laptop' }[p] || 'devices');

/** Initial-cap a phrase that came from data ("daily 03:30" to "Daily 03:30"). */
export const capital = (s) => (s ? s.charAt(0).toUpperCase() + s.slice(1) : s);
