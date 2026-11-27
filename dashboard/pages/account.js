import { call, describe, href, serverUrl, now } from '../api.js';
import {
  h, icon, pageHead, section, list, row, badge, button, busy, field, input, checkbox, empty, agoInline, nextId,
  confirm, toast, failed, secret, formError, setFieldError, platformName, platformIcon, segmented, openDialog, meter,
} from '../ui.js';

const SCOPES = [
  ['library', 'Browse'],
  ['stream', 'Play songs'],
  ['download', 'Download'],
  ['write', 'Edit playlists and favourites'],
];

export async function render(ctx) {
  const [{ devices }, { keys }] = await Promise.all([call('GET /me/devices'), call('GET /me/api-keys')]);
  const name = ctx.user.display_name || ctx.user.username;
  const page = h('div.page.narrow',
    pageHead('My account', { lede: `Signed in as ${ctx.user.username}${name !== ctx.user.username ? ` (${name})` : ''}` }),
    pairSection(ctx, devices),
    devicesSection(ctx, devices),
    keysSection(ctx, keys),
    profileSection(ctx),
    dataSection());
  if (ctx.args[0] === 'pair') setTimeout(() => page.querySelector('#pair-start')?.focus(), 0);
  return page;
}

/* ---------- Add a device ---------- */

function pairSection(ctx, devices) {
  const body = h('div.panel');
  const known = new Set(devices.map((d) => d.id));
  let timer = 0;
  let poll = 0;
  let offer = null;
  const stop = () => { clearTimeout(timer); clearInterval(poll); timer = 0; poll = 0; };
  ctx.onLeave(() => {
    stop();
    if (offer) call('DELETE /me/pairing-codes/{id}', { values: { id: offer.id } }).catch(() => {});
  });

  const idle = () => {
    stop();
    offer = null;
    const start = button('Show pairing code', { kind: 'primary', iconName: 'qr' });
    start.id = 'pair-start';
    start.addEventListener('click', () => busy(start, async () => {
      try {
        offer = await call('POST /me/pairing-codes', { body: {} });
        showOffer();
      } catch (err) { failed(err); }
    }));
    body.replaceChildren(
      h('div.form.tight', h('p', 'Show a code here, then scan it or type it in Tracks on the device you want to add.'), h('div.btn-row', start)),
      h('hr.rule'),
      approveForm('POST /pair/requests/approve', 'Or type the 6-digit code Tracks shows'));
  };

  const showOffer = () => {
    const countdown = h('span.num');
    const status = h('p.small.muted', { role: 'status' });
    const cancel = button('Cancel', {});
    cancel.addEventListener('click', async () => {
      const id = offer?.id;
      idle();
      if (id) call('DELETE /me/pairing-codes/{id}', { values: { id } }).catch(() => {});
    });
    const total = Math.max(1, Date.parse(offer.expires_at) - now());
    const started = performance.now();
    const left = () => Math.max(0, total - (performance.now() - started));
    const bar = meter(1, 1, 'Time left on this code');
    // The bar drains on the compositor at the display's own rate; the text only changes once a second.
    bar.firstChild.animate([{ transform: `scaleX(${left() / total})` }, { transform: 'scaleX(0)' }], { duration: left(), easing: 'linear', fill: 'forwards' });
    // On the device that is being added, the QR is no use: the link opens Tracks directly.
    const openHere = window.matchMedia('(pointer: coarse)').matches && offer.qr_payload
      ? h('a.btn.primary', { href: offer.qr_payload }, 'Open in Tracks on this device') : null;
    body.replaceChildren(h('div.pairing.reveal',
      h('div.qr', h('img', { src: serverUrl(offer.qr_svg_url), alt: `Pairing QR code. The typed code is ${offer.code}.`, width: '176', height: '176' })),
      h('div.form',
        h('p', 'Scan this with the camera in Tracks, or type the code.'),
        h('div', h('div.code-big', { 'aria-label': `Code ${offer.code.split('').join(' ')}` }, offer.code)),
        h('div.countdown', h('p.small.muted', 'Works for another ', countdown), bar),
        status,
        h('div.btn-row', openHere, cancel))));
    const tick = async () => {
      if (!offer) return;
      const ms = left();
      const secs = Math.ceil(ms / 1000);
      countdown.textContent = `${Math.floor(secs / 60)}:${String(secs % 60).padStart(2, '0')}`;
      if (ms <= 0) {
        stop();
        status.textContent = 'This code has expired.';
        countdown.parentElement.hidden = true;
        cancel.replaceChildren('Show a new code');
        return;
      }
      timer = setTimeout(tick, (ms % 1000) || 1000);
    };
    tick();
    poll = setInterval(async () => {
      if (!offer) return;
      const { devices: latest } = await call('GET /me/devices').catch(() => ({ devices: [] }));
      const added = latest.find((d) => !known.has(d.id));
      if (added && offer) {
        stop();
        offer = null;
        toast(`${added.name} is connected`);
        ctx.rerender();
      }
    }, 3000);
  };

  idle();
  return section('Connect a device', body, { id: 'pair' });
}

/** The other direction: the app shows a 6-digit code and someone signed in approves it. */
export function approveForm(route, labelText = 'Code from the app', hint) {
  const code = input({ id: nextId('code'), inputmode: 'numeric', autocomplete: 'one-time-code', maxlength: '7', placeholder: '000 000', mono: true });
  code.classList.add('short');
  const submit = button('Connect', { type: 'submit' });
  const error = formError();
  return h('form.form', {
    on: {
      submit: async (event) => {
        event.preventDefault();
        error.hide();
        setFieldError(code, '');
        const digits = code.value.replace(/\D/g, '');
        if (digits.length !== 6) { setFieldError(code, 'The code has 6 digits.'); code.focus(); return; }
        await busy(submit, async () => {
          try {
            const res = await call(route, { body: { code: digits } });
            code.value = '';
            toast(`${res?.device?.name || 'The device'} is connected`);
          } catch (err) {
            if (err.code === 'not_found' || err.code === 'unauthenticated') setFieldError(code, 'That code is wrong or has expired. Check Tracks for the current one.');
            else error.show(describe(err));
          }
        });
      },
    },
  },
  h('div.field',
    h('label', { for: code.id }, labelText),
    h('div.btn-row', code, submit),
    hint ? h('p.hint', hint) : null,
    h('p.error', { id: `${code.id}-error`, hidden: true })),
  error.el);
}

/* ---------- Devices ---------- */

function devicesSection(ctx, devices) {
  // A TV shows an 8-character code instead of scanning one.
  const tv = h('a.btn', { href: '#/link' }, icon('tv', 'sm'), 'Connect a TV');
  if (!devices.length) return section('Your devices', list(empty('No devices yet', 'Connect one above.')), { aside: tv });
  const sorted = [...devices].sort((a, b) => Date.parse(b.last_seen_at || 0) - Date.parse(a.last_seen_at || 0));
  return section('Your devices', list(sorted.map((d) => {
    const remove = button('Remove', { kind: 'danger' });
    remove.setAttribute('aria-label', `Remove ${d.name}`);
    remove.addEventListener('click', async () => {
      if (!await confirm({ title: `Remove ${d.name}?`, text: 'It is signed out straight away. You can connect it again later.', action: 'Remove' })) return;
      await busy(remove, async () => {
        try {
          await call('DELETE /me/devices/{id}', { values: { id: d.id } });
          toast(`${d.name} removed`);
          ctx.rerender();
        } catch (err) { failed(err); }
      });
    });
    const rename = button('Rename', { kind: 'quiet' });
    rename.setAttribute('aria-label', `Rename ${d.name}`);
    rename.addEventListener('click', () => renameDevice(ctx, d));
    return row({
      icon: platformIcon(d.platform), tint: 'muted',
      title: [d.name, d.current ? badge('This device') : null],
      sub: `${platformName(d.platform)}, used ${agoInline(d.last_seen_at)}`,
      trail: [rename, remove],
      wrapTrail: true,
    });
  }), { iconInset: true }), { aside: tv });
}

function renameDevice(ctx, device) {
  const name = input({ value: device.name, maxlength: '64' });
  const titleId = nextId('d');
  const dialog = h('dialog', { 'aria-labelledby': titleId },
    h('form', { method: 'dialog' },
      h('div.dialog-body', h('h2', { id: titleId }, `Rename ${device.name}`), field('Name', name)),
      h('div.dialog-actions', h('button.btn', { value: 'cancel', formnovalidate: true }, 'Cancel'), h('button.btn.primary', { value: 'ok' }, 'Save'))));
  openDialog(dialog, async (result) => {
    const value = name.value.trim();
    if (result !== 'ok' || !value || value === device.name) return;
    try {
      await call('PATCH /me/devices/{id}', { values: { id: device.id }, body: { name: value } });
      toast('Renamed');
      ctx.rerender();
    } catch (err) { failed(err); }
  });
  name.select();
}

/* ---------- Keys and app passwords ---------- */

function keysSection(ctx, keys) {
  const canAppPassword = Boolean(ctx.user.permissions?.legacy_subsonic);
  const slot = h('div');
  const add = button('New key', { iconName: 'plus' });
  add.addEventListener('click', () => {
    add.hidden = true;
    slot.replaceChildren(newKeyForm(ctx, canAppPassword, () => { add.hidden = false; slot.replaceChildren(); add.focus(); }));
    slot.querySelector('input')?.focus();
  });

  const rows = keys.map((k) => {
    const revoke = button('Revoke', { kind: 'danger' });
    revoke.setAttribute('aria-label', `Revoke ${k.name}`);
    revoke.addEventListener('click', async () => {
      if (!await confirm({ title: `Revoke ${k.name}?`, text: 'Any app using it is signed out straight away.', action: 'Revoke' })) return;
      await busy(revoke, async () => {
        try {
          await call('DELETE /me/api-keys/{id}', { values: { id: k.id } });
          toast(`${k.name} revoked`);
          ctx.rerender();
        } catch (err) { failed(err); }
      });
    });
    return row({
      icon: 'key', tint: 'muted',
      title: [k.name, badge(k.kind === 'app_password' ? 'App password' : 'API key')],
      sub: k.last_used_at ? `Used ${agoInline(k.last_used_at)}` : 'Never used',
      trail: revoke,
    });
  });

  return section('App keys', h('div.form',
    list(rows.length ? rows : [empty('No app keys', canAppPassword ? 'For other music apps and scripts. Tracks does not need one.' : 'For scripts and other tools. Tracks does not need one.')], { iconInset: true }),
    slot), { aside: add });
}

function newKeyForm(ctx, canAppPassword, close) {
  let kind = 'api_key';
  const name = input({ maxlength: '64', placeholder: 'For example, Symfonium', required: true });
  const boxes = SCOPES.map(([value, label]) => checkbox(label, value !== 'download', { value }));
  const error = formError();
  const submit = button('Create key', { kind: 'primary', type: 'submit' });
  const kindHint = h('p.hint', { hidden: true });
  const syncHint = () => {
    kindHint.hidden = kind !== 'app_password';
    kindHint.textContent = kind === 'app_password'
      ? 'For apps that ask for a password.'
      : 'For scripts and apps that take an API key.';
  };
  syncHint();
  const form = h('form.panel.form.reveal', {
    on: {
      submit: async (event) => {
        event.preventDefault();
        error.hide();
        setFieldError(name, '');
        if (!name.value.trim()) { setFieldError(name, 'Name it after the app that will use it.'); name.focus(); return; }
        const scopes = boxes.map((b) => b.querySelector('input')).filter((i) => i.checked).map((i) => i.value);
        if (!scopes.length) { error.show('Choose at least one thing the key can do.'); return; }
        await busy(submit, async () => {
          try {
            const created = await call('POST /me/api-keys', { body: { kind, name: name.value.trim(), scopes } });
            form.replaceWith(h('div.panel.form.reveal',
              h('h2', `${created.name} is ready`),
              created.kind === 'app_password'
                ? h('div.form', field('Username', h('input.input', { readonly: true, value: created.username || ctx.user.username })), secret(created.secret, 'App password'))
                : h('div.form', secret(created.secret, 'API key')),
              h('p.secret-note', 'Copy it now. It is not shown again.'),
              h('div.btn-row.end', button('Done', { kind: 'primary', onClick: () => ctx.rerender() }))));
          } catch (err) { error.show(describe(err)); }
        });
      },
    },
  },
  canAppPassword ? h('div.field', h('span.label', 'Type'),
    segmented('key-kind', [['api_key', 'API key'], ['app_password', 'App password']], kind, (v) => { kind = v; syncHint(); }, 'Type'),
    kindHint) : null,
  field('Name', name),
  h('fieldset.field.plain-fieldset', h('legend.label', 'It can'), h('div.list.check-list', boxes)),
  error.el,
  h('div.btn-row.end', button('Cancel', { onClick: close }), submit));
  return form;
}

/* ---------- Profile ---------- */

function profileSection(ctx) {
  const display = input({ value: ctx.user.display_name || '', autocomplete: 'name' });
  const saveName = button('Save', { type: 'submit' });
  const nameForm = h('form.form', {
    on: {
      submit: async (event) => {
        event.preventDefault();
        await busy(saveName, async () => {
          try {
            await call('PATCH /me', { body: { display_name: display.value.trim() || ctx.user.username } });
            ctx.user.display_name = display.value.trim() || ctx.user.username;
            toast('Saved');
          } catch (err) { failed(err); }
        });
      },
    },
  }, h('div.field', h('label', { for: (display.id = nextId('n')) }, 'Name'), h('div.btn-row.nowrap-row', display, saveName)));

  const current = input({ type: 'password', autocomplete: 'current-password' });
  const next = input({ type: 'password', autocomplete: 'new-password' });
  const signOut = checkbox('Also sign out my devices', false);
  const error = formError();
  const savePw = button('Change password', { type: 'submit' });
  const pwForm = h('form.form', {
    novalidate: true,
    on: {
      submit: async (event) => {
        event.preventDefault();
        error.hide();
        setFieldError(next, '');
        await busy(savePw, async () => {
          try {
            await call('POST /me/password', { body: { current_password: current.value, new_password: next.value, sign_out_devices: signOut.querySelector('input').checked } });
            current.value = '';
            next.value = '';
            toast('Password changed');
          } catch (err) {
            error.show(err.code === 'unauthenticated' || err.code === 'forbidden' ? 'Your current password is wrong.' : describe(err));
          }
        });
      },
    },
  },
  h('div.fields-2', field('Current password', current), field('New password', next)),
  signOut, error.el, h('div.btn-row.end', savePw));

  return section('Profile', h('div.panel.form', nameForm, h('hr.rule'), pwForm));
}

function dataSection() {
  return section('Your data', h('div.list',
    row({
      icon: 'download', tint: 'muted',
      title: 'Listening history',
      sub: 'Every play, in a format ListenBrainz can import',
      trail: h('a.btn', { href: href('GET /me/listens/export'), download: 'listens.jsonl' }, 'Download'),
    })), {});
}

