import { call, describe, href, serverUrl, now } from '../api.js';
import {
  h, pageHead, section, list, row, badge, button, busy, field, input, checkbox, empty, ago, agoInline, nextId,
  confirm, toast, failed, secret, formError, setFieldError, platformName, segmented,
} from '../ui.js';

const SCOPES = [
  ['library', 'Browse'],
  ['stream', 'Play songs'],
  ['download', 'Download'],
  ['write', 'Edit playlists and favourites'],
];

export async function render(ctx) {
  const [{ devices }, { keys }] = await Promise.all([call('GET /me/devices'), call('GET /me/api-keys')]);
  const page = h('div.page.narrow',
    pageHead('My Account'),
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
  let offer = null;
  const stop = () => { clearInterval(timer); timer = 0; };
  ctx.onLeave(() => {
    stop();
    if (offer) call('DELETE /me/pairing-codes/{id}', { values: { id: offer.id } }).catch(() => {});
  });

  const idle = () => {
    stop();
    offer = null;
    const start = button('Show Code', { kind: 'primary', iconName: 'qr' });
    start.id = 'pair-start';
    start.addEventListener('click', () => busy(start, async () => {
      try {
        offer = await call('POST /me/pairing-codes', { body: {} });
        showOffer();
      } catch (err) { failed(err); }
    }));
    body.replaceChildren(
      h('div.btn-row', start),
      approveForm('POST /pair/requests/approve'));
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
    body.replaceChildren(h('div.pairing.reveal',
      h('div.qr', h('img', { src: serverUrl(offer.qr_svg_url), alt: `Pairing QR code. The typed code is ${offer.code}.`, width: '176', height: '176' })),
      h('div.form',
        h('p', 'Scan in Tracks, or type the code.'),
        h('div', h('div.code-big', { 'aria-label': `Code ${offer.code.split('').join(' ')}` }, offer.code)),
        h('p.small.muted', 'Expires in ', countdown),
        status,
        h('div.btn-row', cancel))));
    let ticks = 0;
    const started = performance.now();
    const base = now();
    const tick = async () => {
      if (!offer) return;
      const left = Math.max(0, Math.round((Date.parse(offer.expires_at) - base - (performance.now() - started)) / 1000));
      countdown.textContent = `${Math.floor(left / 60)}:${String(left % 60).padStart(2, '0')}`;
      if (left === 0) {
        stop();
        status.textContent = 'Expired.';
        cancel.lastChild.textContent = 'Start Again';
        return;
      }
      ticks += 1;
      if (ticks % 3 === 0) {
        const { devices: latest } = await call('GET /me/devices').catch(() => ({ devices: [] }));
        const added = latest.find((d) => !known.has(d.id));
        if (added) {
          stop();
          offer = null;
          toast(`${added.name} added`);
          ctx.rerender();
        }
      }
    };
    tick();
    timer = setInterval(tick, 1000);
  };

  idle();
  return section('Pair a Device', body, { id: 'pair' });
}

/** The other direction: the app shows a 6-digit code and someone signed in approves it. */
export function approveForm(route, labelText = 'Code from the app') {
  const code = input({ id: nextId('code'), inputmode: 'numeric', autocomplete: 'one-time-code', maxlength: '7', placeholder: '000 000', mono: true });
  code.classList.add('short');
  const submit = button('Approve', { type: 'submit' });
  const error = formError();
  return h('form.form', {
    on: {
      submit: async (event) => {
        event.preventDefault();
        error.hide();
        setFieldError(code, '');
        const digits = code.value.replace(/\D/g, '');
        if (digits.length !== 6) { setFieldError(code, 'Enter 6 digits.'); code.focus(); return; }
        await busy(submit, async () => {
          try {
            const res = await call(route, { body: { code: digits } });
            code.value = '';
            toast(`${res?.device?.name || 'The device'} added`);
          } catch (err) {
            if (err.code === 'not_found' || err.code === 'unauthenticated') setFieldError(code, 'Wrong or expired code.');
            else error.show(describe(err));
          }
        });
      },
    },
  },
  h('div.field',
    h('label', { for: code.id }, labelText),
    h('div.btn-row', code, submit),
    h('p.error', { id: `${code.id}-error`, hidden: true })),
  error.el);
}

/* ---------- Devices ---------- */

function devicesSection(ctx, devices) {
  if (!devices.length) return section('Devices', list(empty('No devices')));
  const sorted = [...devices].sort((a, b) => Date.parse(b.last_seen_at || 0) - Date.parse(a.last_seen_at || 0));
  return section('Devices', list(sorted.map((d) => {
    const remove = button('Remove', { kind: 'danger' });
    remove.setAttribute('aria-label', `Remove ${d.name}`);
    remove.addEventListener('click', async () => {
      if (!await confirm({ title: `Remove ${d.name}?`, text: 'It is signed out now.', action: 'Remove' })) return;
      await busy(remove, async () => {
        try {
          await call('DELETE /me/devices/{id}', { values: { id: d.id } });
          toast(`${d.name} removed`);
          ctx.rerender();
        } catch (err) { failed(err); }
      });
    });
    const rename = button('Rename', {});
    rename.setAttribute('aria-label', `Rename ${d.name}`);
    rename.addEventListener('click', () => renameDevice(ctx, d));
    return row({
      icon: 'devices', tint: 'indigo',
      title: [d.name, d.current ? badge('This device') : null],
      sub: `${platformName(d.platform)}, ${agoInline(d.last_seen_at)}`,
      trail: [rename, remove],
      wrapTrail: true,
    });
  }), { iconInset: true }));
}

function renameDevice(ctx, device) {
  const name = input({ value: device.name, maxlength: '64', 'aria-label': 'Device name' });
  const dialog = h('dialog', { 'aria-label': 'Rename device' },
    h('form', { method: 'dialog' },
      h('div.dialog-body', h('h2', 'Rename device'), field('Name', name)),
      h('div.dialog-actions', h('button.btn', { value: 'cancel', formnovalidate: true }, 'Cancel'), h('button.btn.primary', { value: 'ok' }, 'Save'))));
  document.body.append(dialog);
  dialog.addEventListener('close', async () => {
    const value = name.value.trim();
    dialog.remove();
    if (dialog.returnValue !== 'ok' || !value || value === device.name) return;
    try {
      await call('PATCH /me/devices/{id}', { values: { id: device.id }, body: { name: value } });
      toast('Renamed');
      ctx.rerender();
    } catch (err) { failed(err); }
  });
  dialog.showModal();
  name.select();
}

/* ---------- Keys and app passwords ---------- */

function keysSection(ctx, keys) {
  const canAppPassword = Boolean(ctx.user.permissions?.legacy_subsonic);
  const slot = h('div');
  const add = button('New Key', { iconName: 'plus' });
  add.addEventListener('click', () => {
    add.hidden = true;
    slot.replaceChildren(newKeyForm(ctx, canAppPassword, () => { add.hidden = false; slot.replaceChildren(); add.focus(); }));
    slot.querySelector('input')?.focus();
  });

  const rows = keys.map((k) => {
    const revoke = button('Revoke', { kind: 'danger' });
    revoke.setAttribute('aria-label', `Revoke ${k.name}`);
    revoke.addEventListener('click', async () => {
      if (!await confirm({ title: `Revoke ${k.name}?`, text: 'Apps using it are signed out now.', action: 'Revoke' })) return;
      await busy(revoke, async () => {
        try {
          await call('DELETE /me/api-keys/{id}', { values: { id: k.id } });
          toast('Revoked');
          ctx.rerender();
        } catch (err) { failed(err); }
      });
    });
    return row({
      icon: 'key', tint: 'indigo',
      title: [k.name, badge(k.kind === 'app_password' ? 'App password' : 'Key')],
      sub: k.last_used_at ? `Used ${agoInline(k.last_used_at)}` : 'Never used',
      trail: revoke,
    });
  });

  return section('App Keys', h('div.form',
    list(rows.length ? rows : [empty('No keys')], { iconInset: true }),
    slot,
    h('div.btn-row', add)));
}

function newKeyForm(ctx, canAppPassword, close) {
  let kind = 'api_key';
  const name = input({ maxlength: '64', placeholder: 'Symfonium…', required: true });
  const boxes = SCOPES.map(([value, label]) => checkbox(label, value !== 'download', { value }));
  const error = formError();
  const submit = button('Create', { kind: 'primary', type: 'submit' });
  const kindHint = h('p.hint', { hidden: true });
  const syncHint = () => {
    kindHint.hidden = kind !== 'app_password';
    kindHint.textContent = kind === 'app_password'
      ? 'For apps that ask for a password.'
      : '';
  };
  syncHint();
  const form = h('form.panel.form.reveal', {
    on: {
      submit: async (event) => {
        event.preventDefault();
        error.hide();
        setFieldError(name, '');
        if (!name.value.trim()) { setFieldError(name, 'Add a name.'); name.focus(); return; }
        const scopes = boxes.map((b) => b.querySelector('input')).filter((i) => i.checked).map((i) => i.value);
        if (!scopes.length) { error.show('Choose at least one.'); return; }
        await busy(submit, async () => {
          try {
            const created = await call('POST /me/api-keys', { body: { kind, name: name.value.trim(), scopes } });
            form.replaceWith(h('div.panel.form.reveal',
              h('h2', `${created.name} is ready`),
              created.kind === 'app_password'
                ? h('div.form', field('Username', h('input.input', { readonly: true, value: created.username || ctx.user.username })), secret(created.secret, 'App password'))
                : h('div.form', secret(created.secret, 'Key')),
              h('p.secret-note', 'Shown once.'),
              h('div.btn-row.end', button('Done', { kind: 'primary', onClick: () => ctx.rerender() }))));
          } catch (err) { error.show(describe(err)); }
        });
      },
    },
  },
  canAppPassword ? h('div.field', h('span.label', 'Kind'),
    segmented('key-kind', [['api_key', 'Key'], ['app_password', 'App password']], kind, (v) => { kind = v; syncHint(); }, 'Kind'),
    kindHint) : null,
  field('Name', name),
  h('fieldset.field.plain-fieldset', h('legend.label', 'Allow'), h('div.list.check-list', boxes)),
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
  const signOut = checkbox('Sign out my devices', false);
  const error = formError();
  const savePw = button('Change Password', { type: 'submit' });
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
            error.show(err.code === 'unauthenticated' || err.code === 'forbidden' ? 'Wrong current password.' : describe(err));
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
  return section('Your Data', h('div.list',
    row({
      icon: 'download', tint: 'accent',
      title: 'Listening history',
      trail: h('a.btn', { href: href('GET /me/listens/export'), download: 'listens.jsonl' }, 'Download'),
    })), {});
}

